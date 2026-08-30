//! Client de l'agent, partagé par le serveur MCP et le lanceur.
//!
//! Les deux ont le même besoin : parler à l'agent, le démarrer s'il ne tourne
//! pas encore. Cette logique ne dépend d'aucun des deux — elle vit ici plutôt
//! que d'être écrite deux fois, une pour chaque binaire.

use crate::agent_protocol::{self, AgentHandle, AgentRequest, AgentResponse};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// S'assure que l'agent tourne, et renvoie le port où le joindre.
///
/// Lit la fiche laissée par un agent déjà en marche ; si elle est absente ou
/// périmée (processus mort sans avoir pu nettoyer), lance un nouvel agent en
/// détaché et attend qu'il écrive sa fiche.
pub async fn ensure_running() -> Result<u16, String> {
    let state_dir = crate::state_dir();
    if let Some(port) = read_live_handle(&state_dir).await {
        return Ok(port);
    }

    let bin = agent_binary_path()?;
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::launch::hide_console(&mut cmd);
    #[cfg(unix)]
    {
        // Détaché de la session courante : l'agent doit survivre à l'appel
        // MCP qui l'a démarré, pas seulement au processus qui l'a lancé.
        // `tokio::process::Command` porte sa propre `pre_exec` sous Unix : le
        // trait de la bibliothèque standard n'a pas à être importé.
        unsafe {
            cmd.pre_exec(|| {
                libc_setsid();
                Ok(())
            });
        }
    }
    cmd.spawn()
        .map_err(|e| format!("{} : {e}", bin.display()))?;

    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Some(port) = read_live_handle(&state_dir).await {
            return Ok(port);
        }
    }
    Err("l'agent Cluster n'a pas démarré à temps (5 s)".to_string())
}

#[cfg(unix)]
fn libc_setsid() {
    extern "C" {
        fn setsid() -> i32;
    }
    unsafe {
        setsid();
    }
}

async fn read_live_handle(state_dir: &std::path::Path) -> Option<u16> {
    let text = std::fs::read_to_string(agent_protocol::handle_file(state_dir)).ok()?;
    let handle: AgentHandle = serde_json::from_str(&text).ok()?;
    // Une fiche présente ne garantit rien : le processus peut être mort sans
    // avoir pu l'effacer (coupure de courant, arrêt forcé). On vérifie en se
    // connectant vraiment plutôt que de faire confiance au fichier seul.
    TcpStream::connect(("127.0.0.1", handle.control_port))
        .await
        .ok()?;
    Some(handle.control_port)
}

fn agent_binary_path() -> Result<PathBuf, String> {
    let exe = if cfg!(windows) {
        "locaryn-cluster-agent.exe"
    } else {
        "locaryn-cluster-agent"
    };
    if let Some(dir) = std::env::var_os("LOCARYN_PLUGIN_BIN_DIR").map(PathBuf::from) {
        let candidat = dir.join(exe);
        if candidat.is_file() {
            return Ok(candidat);
        }
    }
    // À défaut de la variable d'environnement, l'agent est un voisin du
    // binaire courant : les trois programmes de cette extension sont
    // toujours installés côte à côte dans le même `bin/`.
    let voisin = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(exe)));
    match voisin {
        Some(p) if p.is_file() => Ok(p),
        _ => Err(format!(
            "« {exe} » introuvable à côté de ce binaire ni dans LOCARYN_PLUGIN_BIN_DIR"
        )),
    }
}

/// Envoie une requête à l'agent et attend sa réponse.
pub async fn send(req: &AgentRequest) -> Result<AgentResponse, String> {
    let port = ensure_running().await?;
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|e| format!("agent injoignable sur 127.0.0.1:{port} : {e}"))?;
    let bytes = agent_protocol::encode_request(req);
    stream.write_all(&bytes).await.map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(30), reader.read_line(&mut line))
        .await
        .map_err(|_| "l'agent n'a pas répondu (délai dépassé)".to_string())?
        .map_err(|e| e.to_string())?;
    serde_json::from_str(line.trim()).map_err(|e| format!("réponse de l'agent illisible : {e}"))
}
