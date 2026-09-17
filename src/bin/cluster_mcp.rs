//! Serveur MCP stdio de l'extension Cluster.
//!
//! `stdout` est réservé au JSON-RPC ; tout diagnostic passe par `stderr`.
//!
//! Chaque outil est un client fin de l'agent ([`locaryn_plugin_cluster::agent_client`]) :
//! la logique réseau, le pairage et le calcul de répartition vivent dans
//! l'agent, ce serveur ne fait que relayer.
//!
//! **Le démarrage du moteur n'est pas un outil.** Il appartient à Réglages →
//! Moteur, qui enregistre le modèle actif et supervise le processus — deux
//! chemins pour la même action donneraient deux propriétaires au même port.

use locaryn_plugin_cluster::agent_client;
use locaryn_plugin_cluster::agent_protocol::{AgentRequest, AgentResponse};
use serde_json::{json, Value};
use std::io::Write;
use tokio::io::{AsyncBufReadExt, BufReader};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() {
    let mut lignes = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(ligne)) = lignes.next_line().await {
        if ligne.trim().is_empty() {
            continue;
        }
        let reponse = match serde_json::from_str::<Value>(&ligne) {
            Ok(requete) => handle_request(requete).await,
            Err(erreur) => error_response(Value::Null, -32700, format!("JSON invalide : {erreur}")),
        };
        if reponse.is_null() {
            continue;
        }
        if let Ok(serialise) = serde_json::to_string(&reponse) {
            println!("{serialise}");
            let _ = std::io::stdout().flush();
        }
    }
}

async fn handle_request(requete: Value) -> Value {
    let id = requete.get("id").cloned().unwrap_or(Value::Null);
    let methode = requete
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match methode {
        "initialize" => success(
            id,
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "morph-cluster", "version": VERSION }
            }),
        ),
        "tools/list" => success(id, tools_list()),
        "tools/call" => {
            let params = requete.get("params").cloned().unwrap_or_else(|| json!({}));
            let nom = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match call_tool(nom, args).await {
                Ok(valeur) => success(id, text_content(valeur)),
                Err(erreur) => error_response(id, -32000, erreur),
            }
        }
        notification if notification.starts_with("notifications/") => Value::Null,
        _ => error_response(id, -32601, format!("méthode MCP inconnue : {methode}")),
    }
}

fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "cluster_status",
                "description": "État de cette machine dans le cluster : nom du cluster (s'il y en a un), GPU et VRAM libre détectés, présence de llama-server et ggml-rpc-server, nombre de pairs connus. À appeler en premier — avant de proposer de créer ou rejoindre un cluster, ou quand une répartition échoue.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "cluster_create",
                "description": "Crée un nouveau cluster sur cette machine et renvoie le code de pairage à copier sur chaque autre machine (cluster_join). Remplace le cluster actuel s'il y en avait un — prévenir l'utilisateur avant d'appeler si c'est le cas (cluster_status le dit).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Nom du cluster, pour s'y retrouver (ex. « bureau », « atelier »)." }
                    },
                    "required": ["name"]
                }
            },
            {
                "name": "cluster_join",
                "description": "Rejoint un cluster existant à partir d'un code de pairage obtenu sur une autre machine (cluster_create, cluster_pairing_code ou cluster_enroll). Le code contient un secret : ne jamais le faire transiter par un canal non demandé par l'utilisateur.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "pairing_code": { "type": "string", "description": "Code renvoyé par cluster_create, cluster_pairing_code ou cluster_enroll sur une autre machine." },
                        "coordinator": { "type": "string", "description": "Hôte du serveur Locaryn qui a donné le code, quand ce poste y est connecté. Fait de ce poste un client de ce coordinateur : il le joint directement et copie ses modèles partagés si le partage le permet." }
                    },
                    "required": ["pairing_code"]
                }
            },
            {
                "name": "cluster_enroll",
                "description": "Sur la machine du serveur Locaryn : crée le cluster s'il n'existe pas, fait de cette machine le coordinateur (celle qui tient les modèles partagés) et renvoie le code d'inscription d'un poste client. Appelé par le panneau de partage d'un poste client au travers de son compte sur le serveur — un modèle ne l'appelle que si l'utilisateur veut inscrire une machine.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "cluster_leave",
                "description": "Fait quitter le cluster à cette machine : oublie le secret, arrête de prêter ses ressources et supprime les copies de modèles partagés qu'elle avait faites. Demander confirmation avant.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "cluster_share_get",
                "description": "Ce que cette machine accepte de prêter au cluster (carte graphique, mémoire vive, stockage et son quota), les appareils de calcul que llama.cpp y détecte, et si son serveur RPC tourne.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "cluster_share_set",
                "description": "Enregistre et applique ce que cette machine prête au cluster. C'est le choix de la personne devant la machine : ne l'appeler que sur sa demande explicite.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "enabled": { "type": "boolean", "description": "Allouer cette machine au partage de ressources. Faux : rien n'est prêté." },
                        "gpu": { "type": "boolean", "description": "Prêter la carte graphique." },
                        "ram": { "type": "boolean", "description": "Prêter la mémoire vive." },
                        "storage": { "type": "boolean", "description": "Héberger une copie des modèles partagés." },
                        "storage_gb": { "type": "number", "description": "Place maximale accordée aux copies, en Go." }
                    },
                    "required": ["enabled"]
                }
            },
            {
                "name": "cluster_shared_models",
                "description": "Sur le coordinateur : les modèles GGUF de la bibliothèque, lesquels sont partagés, et quelles machines en ont déjà une copie ; plus les machines du cluster et ce qu'elles prêtent.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "cluster_share_model",
                "description": "Sur le coordinateur : partager un modèle de la bibliothèque (les postes clients qui hébergent des modèles en font une copie) ou cesser de le partager (leurs copies sont supprimées). Ne l'appeler que sur demande explicite.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Chemin du fichier GGUF relatif à la bibliothèque, tel que renvoyé par cluster_shared_models." },
                        "enabled": { "type": "boolean", "description": "Vrai pour partager, faux pour cesser." }
                    },
                    "required": ["file", "enabled"]
                }
            },
            {
                "name": "cluster_sync_status",
                "description": "Sur un poste client : les modèles que le coordinateur partage et l'état de chacun sur ce poste (copié, copie en cours avec progression, en attente, place insuffisante, non hébergé).",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "cluster_pairing_code",
                "description": "Le code de pairage du cluster actuel, à copier sur une machine qui doit le rejoindre. Contient le secret du cluster : à ne montrer que si l'utilisateur veut explicitement ajouter une machine.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "cluster_peers",
                "description": "Les machines actuellement connues du cluster : adresse, GPU, VRAM libre, dernière fois vues, latence mesurée. Vide si aucune autre machine n'a encore été détectée sur le réseau — la découverte prend jusqu'à une dizaine de secondes après le démarrage de l'agent sur les deux machines.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "cluster_worker_start",
                "description": "Fait offrir son GPU par cette machine au cluster (démarre ggml-rpc-server). À appeler sur une machine qui doit PRÊTER de la VRAM, pas sur celle qui pilotera la conversation.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "cluster_worker_stop",
                "description": "Arrête de prêter le GPU de cette machine au cluster et rend la VRAM.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "cluster_plan",
                "description": "Calcule si les pairs actuellement joignables ont assez de VRAM cumulée pour un modèle de cette taille, et lesquels seraient retenus. À appeler avant de choisir ce moteur dans Réglages → Moteur, pour prévenir l'utilisateur si la capacité manque — le lancement fera le même calcul et refusera pour la même raison.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "model_size_gb": { "type": "number", "description": "Taille du fichier GGUF visé, en gigaoctets." }
                    },
                    "required": ["model_size_gb"]
                }
            },
            {
                "name": "cluster_bench",
                "description": "Mesure l'aller-retour de contrôle vers un pair. Un lien lent (Wi-Fi faible, liaison à forte latence) peut rendre la répartition RPC plus lente que l'exécution locale seule — ce nombre le dit avant de lancer un chargement de plusieurs dizaines de gigaoctets.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "peer_id": { "type": "string", "description": "Identifiant du pair, tel que renvoyé par cluster_peers." }
                    },
                    "required": ["peer_id"]
                }
            },
            {
                "name": "cluster_protocols",
                "description": "Les protocoles de calcul distribué que cette extension connaît, et lesquels sont réellement implémentés aujourd'hui — à consulter avant de promettre à l'utilisateur un protocole qui n'est encore qu'à l'étude.",
                "inputSchema": { "type": "object", "properties": {} }
            }
        ]
    })
}

async fn call_tool(nom: &str, args: Value) -> Result<Value, String> {
    match nom {
        "cluster_status" => match agent_client::send(&AgentRequest::Status).await? {
            AgentResponse::Status {
                cluster_name,
                cluster_id,
                self_capability,
                worker_active,
                rpc_port,
                peer_count,
            } => Ok(json!({
                "cluster_name": cluster_name,
                "cluster_id": cluster_id,
                "gpu": self_capability.gpu_name,
                "vram_libre_gio": self_capability.free_vram_gb,
                "llama_server_present": self_capability.has_llama_server,
                "ggml_rpc_server_present": self_capability.has_rpc_server,
                "offre_son_gpu": worker_active,
                "port_rpc_local": rpc_port,
                "pairs_connus": peer_count,
                "note": if self_capability.has_llama_server {
                    Value::Null
                } else {
                    Value::String(
                        "llama-server absent — ce moteur ne pourra pas démarrer. \
                         Réglages → Moteur → Cluster GPU installera llama.cpp au premier \
                         lancement.".to_string()
                    )
                }
            })),
            other => unexpected(other),
        },
        "cluster_create" => {
            let name = args
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .ok_or("« name » est requis")?;
            match agent_client::send(&AgentRequest::CreateCluster {
                name: name.to_string(),
            })
            .await?
            {
                AgentResponse::Created { pairing_code } => Ok(json!({
                    "cree": true,
                    "nom": name,
                    "code_de_pairage": pairing_code,
                    "instructions": "Copiez ce code tel quel sur chaque autre machine, et appelez \
                                     cluster_join avec. Le code contient un secret : ne le \
                                     partagez que sur un canal de confiance."
                })),
                other => unexpected(other),
            }
        }
        "cluster_join" => {
            let code = args
                .get("pairing_code")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .ok_or("« pairing_code » est requis")?;
            let coordinator = args
                .get("coordinator")
                .and_then(Value::as_str)
                .map(str::to_string);
            match agent_client::send(&AgentRequest::JoinCluster {
                pairing_code: code.to_string(),
                coordinator,
            })
            .await?
            {
                AgentResponse::Joined { cluster_name } => Ok(json!({
                    "rejoint": true,
                    "nom": cluster_name
                })),
                other => unexpected(other),
            }
        }
        "cluster_enroll" => match agent_client::send(&AgentRequest::Enroll).await? {
            AgentResponse::Enrolled {
                cluster_name,
                pairing_code,
            } => Ok(json!({
                "cluster": cluster_name,
                "code_de_pairage": pairing_code
            })),
            other => unexpected(other),
        },
        "cluster_leave" => match agent_client::send(&AgentRequest::Leave).await? {
            AgentResponse::Left => Ok(json!({ "quitte": true })),
            other => unexpected(other),
        },
        "cluster_share_get" => as_value(agent_client::send(&AgentRequest::ShareGet).await?),
        "cluster_share_set" => {
            let actuel = match agent_client::send(&AgentRequest::ShareGet).await? {
                AgentResponse::Share { prefs, .. } => prefs,
                other => return unexpected(other),
            };
            let prefs = merge_prefs(actuel, &args)?;
            as_value(agent_client::send(&AgentRequest::ShareSet { prefs }).await?)
        }
        "cluster_shared_models" => as_value(agent_client::send(&AgentRequest::SharedModels).await?),
        "cluster_share_model" => {
            let file = args
                .get("file")
                .and_then(Value::as_str)
                .filter(|f| !f.is_empty())
                .ok_or("« file » est requis")?
                .to_string();
            let enabled = args
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or("« enabled » est requis")?;
            as_value(agent_client::send(&AgentRequest::ShareModel { file, enabled }).await?)
        }
        "cluster_sync_status" => as_value(agent_client::send(&AgentRequest::SyncStatus).await?),
        "cluster_pairing_code" => match agent_client::send(&AgentRequest::PairingCode).await? {
            AgentResponse::PairingCode { pairing_code } => Ok(json!({
                "code_de_pairage": pairing_code
            })),
            other => unexpected(other),
        },
        "cluster_peers" => match agent_client::send(&AgentRequest::Peers).await? {
            AgentResponse::Peers { peers } => Ok(json!({ "pairs": peers })),
            other => unexpected(other),
        },
        "cluster_worker_start" => match agent_client::send(&AgentRequest::WorkerStart).await? {
            AgentResponse::WorkerStarted { port } => Ok(json!({
                "demarre": true,
                "port_rpc": port
            })),
            other => unexpected(other),
        },
        "cluster_worker_stop" => match agent_client::send(&AgentRequest::WorkerStop).await? {
            AgentResponse::WorkerStopped => Ok(json!({ "arrete": true })),
            other => unexpected(other),
        },
        "cluster_plan" => {
            let taille = args
                .get("model_size_gb")
                .and_then(Value::as_f64)
                .ok_or("« model_size_gb » est requis")?;
            match agent_client::send(&AgentRequest::Plan {
                model_size_gb: taille as f32,
            })
            .await?
            {
                AgentResponse::Plan { plan } => Ok(serde_json::to_value(plan).unwrap_or(json!({}))),
                other => unexpected(other),
            }
        }
        "cluster_bench" => {
            let peer_id = args
                .get("peer_id")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
                .ok_or("« peer_id » est requis")?;
            match agent_client::send(&AgentRequest::Bench {
                peer_id: peer_id.to_string(),
            })
            .await?
            {
                AgentResponse::Benched {
                    peer_id,
                    round_trip_ms,
                } => Ok(json!({
                    "peer_id": peer_id,
                    "aller_retour_ms": round_trip_ms
                })),
                other => unexpected(other),
            }
        }
        "cluster_protocols" => Ok(protocols_status()),
        autre => Err(format!("outil inconnu : {autre}")),
    }
}

/// Ce que cette extension sait faire aujourd'hui — et ce qu'elle ne fait pas
/// encore. Un modèle qui n'a jamais lu ce tableau promet facilement un
/// protocole non implémenté ; ce tableau existe pour que la réponse honnête
/// soit la plus simple à donner, pas la plus difficile.
fn protocols_status() -> Value {
    json!({
        "protocoles": [
            {
                "id": "llama_cpp_rpc",
                "nom": "llama.cpp RPC (ggml-rpc-server)",
                "statut": "implemente",
                "formats": ["gguf"],
                "description": "Répartition des couches d'un modèle GGUF entre plusieurs \
                                 machines. C'est ce que cette extension utilise réellement — \
                                 cluster_plan, cluster_worker_start et le moteur « Cluster GPU » \
                                 en dépendent."
            },
            {
                "id": "exo",
                "nom": "Exo (exo-explore)",
                "statut": "non_implemente",
                "description": "Partitionnement automatique sur matériel hétérogène (Apple \
                                 Silicon, NVIDIA). À l'étude, pas de code dans cette extension — \
                                 ne pas annoncer qu'il fonctionne."
            },
            {
                "id": "vllm_ray",
                "nom": "vLLM multi-nœud (Ray + NCCL)",
                "statut": "non_implemente",
                "description": "Parallélisme tenseur/pipeline haute performance, adapté à du \
                                 matériel identique relié par un lien rapide (ex. plusieurs \
                                 machines NVIDIA GB10 reliées en ConnectX). À l'étude, pas de \
                                 code dans cette extension."
            },
            {
                "id": "petals_hivemind",
                "nom": "Petals / hivemind (DHT)",
                "statut": "non_implemente",
                "description": "Calcul distribué résilient façon volontariat, tolérant aux \
                                 machines qui vont et viennent. À l'étude, pas de code dans \
                                 cette extension."
            }
        ],
        "note": "Seul « llama_cpp_rpc » répond aux outils de cette extension. Les autres sont un \
                 état des lieux, pas une fonctionnalité — un appel qui en dépendrait serait \
                 refusé."
    })
}

/// Les réponses structurées de l'agent passent telles quelles : le panneau
/// de partage les lit champ par champ, sans traduction à maintenir ici.
fn as_value(response: AgentResponse) -> Result<Value, String> {
    match response {
        AgentResponse::Error { message } => Err(message),
        other => serde_json::to_value(other).map_err(|e| e.to_string()),
    }
}

/// Applique aux préférences actuelles les seuls champs fournis : un appel
/// qui ne coche que la case principale ne doit pas remettre le quota à zéro.
fn merge_prefs(
    mut prefs: locaryn_plugin_cluster::sharing::SharePrefs,
    args: &Value,
) -> Result<locaryn_plugin_cluster::sharing::SharePrefs, String> {
    prefs.enabled = args
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or("« enabled » est requis")?;
    if let Some(v) = args.get("gpu").and_then(Value::as_bool) {
        prefs.gpu = v;
    }
    if let Some(v) = args.get("ram").and_then(Value::as_bool) {
        prefs.ram = v;
    }
    if let Some(v) = args.get("storage").and_then(Value::as_bool) {
        prefs.storage = v;
    }
    if let Some(v) = args.get("storage_gb").and_then(Value::as_f64) {
        if !(0.0..=100_000.0).contains(&v) {
            return Err("« storage_gb » doit être compris entre 0 et 100 000".to_string());
        }
        prefs.storage_gb = v as f32;
    }
    Ok(prefs)
}

fn unexpected(response: AgentResponse) -> Result<Value, String> {
    if let AgentResponse::Error { message } = response {
        Err(message)
    } else {
        Err("réponse inattendue de l'agent".to_string())
    }
}

// ============================================================================
// Enveloppes JSON-RPC
// ============================================================================

fn text_content(valeur: Value) -> Value {
    let texte = match &valeur {
        Value::String(s) => s.clone(),
        autre => serde_json::to_string_pretty(autre).unwrap_or_else(|_| autre.to_string()),
    };
    json!({ "content": [{ "type": "text", "text": texte }] })
}

fn success(id: Value, resultat: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": resultat })
}

fn error_response(id: Value, code: i32, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() }
    })
}
