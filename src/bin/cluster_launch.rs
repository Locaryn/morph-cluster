//! Lanceur du moteur Cluster — le programme que le socle nomme dans
//! `engine.lifecycle.start`.
//!
//! À chaque démarrage : s'assure que l'agent tourne, lui demande le plan de
//! répartition courant pour ce modèle, s'assure que les pairs retenus ont
//! bien démarré leur `ggml-rpc-server`, puis lance `llama-server --rpc …`
//! et **devient** ce processus sous Linux/macOS (comme le lanceur FreeToken :
//! un maillon de moins entre ce que le socle surveille et ce qui répond
//! vraiment). Sous Windows, `llama-server.exe` n'a pas d'équivalent à
//! `exec()` ; le lanceur reste en surveillance et relaie le code de sortie.

use locaryn_plugin_cluster as cluster;
use locaryn_plugin_cluster::agent_client;
use locaryn_plugin_cluster::agent_protocol::{AgentRequest, AgentResponse};
use locaryn_plugin_cluster::discovery::{ControlRequest, ControlResponse};
use locaryn_plugin_cluster::identity::ClusterIdentity;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..]).await,
        Some("--help") | Some("-h") | None => {
            eprintln!(
                "locaryn-cluster-launch — lance llama-server réparti sur le cluster\n\n\
                   serve --port <n> --model <chemin.gguf>\n"
            );
            ExitCode::SUCCESS
        }
        Some(autre) => {
            eprintln!("[cluster] commande inconnue : {autre}");
            ExitCode::FAILURE
        }
    }
}

fn flag(args: &[String], nom: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == nom {
            return it.next().cloned();
        }
        if let Some(reste) = a.strip_prefix(&format!("{nom}=")) {
            return Some(reste.to_string());
        }
    }
    None
}

async fn serve(args: &[String]) -> ExitCode {
    let port: u16 = flag(args, "--port")
        .and_then(|p| p.parse().ok())
        .unwrap_or(cluster::DEFAULT_ENGINE_PORT);
    let model = flag(args, "--model").unwrap_or_default();

    if model.trim().is_empty() {
        eprintln!("[cluster] aucun modèle : choisissez un fichier GGUF dans Réglages → Moteur.");
        return ExitCode::FAILURE;
    }
    let model_path = std::path::Path::new(&model);
    if !model_path.is_file() {
        eprintln!("[cluster] fichier introuvable : {model}");
        return ExitCode::FAILURE;
    }
    let model_size_gb = model_path
        .metadata()
        .map(|m| m.len() as f32 / 1_073_741_824.0)
        .unwrap_or(0.0);

    // Le plan courant : quels pairs, dans quel ordre. Sans cluster créé, la
    // liste des pairs est vide et le plan ne compte que la machine locale —
    // ce lanceur reste alors un llama-server ordinaire, ce qui est le
    // comportement honnête quand personne n'a rejoint.
    let plan = match agent_client::send(&AgentRequest::Plan { model_size_gb }).await {
        Ok(AgentResponse::Plan { plan }) => plan,
        Ok(AgentResponse::Error { message }) => {
            eprintln!("[cluster] agent : {message}");
            return ExitCode::FAILURE;
        }
        Ok(_) => {
            eprintln!("[cluster] réponse inattendue de l'agent");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("[cluster] agent injoignable : {e}");
            return ExitCode::FAILURE;
        }
    };

    if !plan.sufficient {
        eprintln!(
            "[cluster] capacité insuffisante : {:.1} Go utilisables pour un modèle de {:.1} Go \
             (il manque {:.1} Go). Ajoutez un pair (cluster_peers), ou faites-lui démarrer son \
             GPU (cluster_worker_start) avant de réessayer.",
            plan.usable_capacity_gb, plan.model_size_gb, plan.shortfall_gb
        );
        return ExitCode::FAILURE;
    }

    for exclu in &plan.excluded {
        eprintln!("[cluster] pair {} écarté : {}", exclu.peer_id, exclu.reason);
    }

    // S'assurer que chaque pair retenu a bien démarré son serveur RPC — le
    // plan reflète l'état connu au moment du calcul, qui peut dater de
    // plusieurs balises. Un pair qui a arrêté le sien entre-temps est ici
    // relancé plutôt que de faire échouer tout le chargement.
    //
    // Charge l'identité directement depuis le disque : ce lanceur est un
    // processus séparé de l'agent, mais le secret du cluster n'a pas besoin
    // de transiter par lui pour ça — il vit dans le même dossier d'état, lu
    // par l'utilisateur qui exécute Locaryn, comme l'agent le fait lui-même.
    let identity = ClusterIdentity::load(&cluster::state_dir());
    for membre in &plan.members {
        let Some(identity) = &identity else {
            eprintln!(
                "[cluster] {} : aucune identité de cluster localement — impossible de \
                 s'authentifier auprès des pairs",
                membre.peer_id
            );
            break;
        };
        if let Err(e) = ensure_peer_rpc(&membre.address, &identity.secret).await {
            eprintln!(
                "[cluster] {} ({}) n'a pas pu démarrer son serveur RPC : {e} — exclu de ce \
                 lancement",
                membre.peer_id, membre.address
            );
        }
    }

    let bin = match locaryn_plugin_cluster::launch::find_binary("llama-server") {
        Some(b) => b,
        None => {
            eprintln!(
                "[cluster] llama-server introuvable. Appelez l'outil cluster_status pour voir \
                 comment l'installer."
            );
            return ExitCode::FAILURE;
        }
    };

    let mut cmd_args = vec![
        "--host".to_string(),
        "127.0.0.1".to_string(),
        "--port".to_string(),
        port.to_string(),
        "-m".to_string(),
        model,
    ];
    let rpc_arg = plan.rpc_argument();
    if !rpc_arg.is_empty() {
        cmd_args.push("--rpc".to_string());
        cmd_args.push(rpc_arg.clone());
        eprintln!("[cluster] --rpc {rpc_arg}");
    } else {
        eprintln!(
            "[cluster] aucun pair retenu — exécution locale, comme un llama-server ordinaire"
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let mut c = std::process::Command::new(&bin);
        c.args(&cmd_args);
        let erreur = c.exec();
        eprintln!("[cluster] {} : {erreur}", bin.display());
        ExitCode::FAILURE
    }

    #[cfg(windows)]
    {
        let mut c = tokio::process::Command::new(&bin);
        c.args(&cmd_args).stdin(std::process::Stdio::null());
        locaryn_plugin_cluster::launch::hide_console(&mut c);
        match c.spawn() {
            Ok(mut enfant) => match enfant.wait().await {
                Ok(s) if s.success() => ExitCode::SUCCESS,
                Ok(s) => {
                    eprintln!("[cluster] llama-server s'est arrêté ({s})");
                    ExitCode::FAILURE
                }
                Err(e) => {
                    eprintln!("[cluster] attente de llama-server impossible : {e}");
                    ExitCode::FAILURE
                }
            },
            Err(e) => {
                eprintln!("[cluster] {} : {e}", bin.display());
                ExitCode::FAILURE
            }
        }
    }
}

/// Demande à un pair de démarrer son serveur RPC s'il ne tourne pas déjà.
///
/// Chaque connexion au canal de contrôle d'un pair repasse par la poignée de
/// main HMAC — le répondant l'exige avant d'accepter quoi que ce soit
/// d'autre, la connaissance du secret ne se transmet pas d'une connexion à
/// l'autre.
async fn ensure_peer_rpc(address: &str, secret: &[u8]) -> Result<(), String> {
    let reponse =
        cluster::peer_channel::authenticated_request(address, secret, &ControlRequest::StartRpc)
            .await?;
    match reponse {
        ControlResponse::RpcStarted { .. } => Ok(()),
        ControlResponse::Error { message } => Err(message),
        _ => Err("réponse inattendue".to_string()),
    }
}
