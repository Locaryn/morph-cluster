//! Une requête de contrôle authentifiée vers un pair, de bout en bout.
//!
//! Toute connexion au canal de contrôle d'un pair commence par la poignée de
//! main HMAC ([`crate::hmac_auth`]) — le répondant (voir `cluster_agent`,
//! `handle_incoming_peer`) l'exige avant d'accepter quoi que ce soit d'autre.
//! Ce module fait les deux étapes dans l'ordre, une seule fois, pour que
//! l'agent et le lanceur ne aient pas chacun leur propre version — une
//! première tentative avait envoyé la requête sans la poignée de main, ce que
//! le répondant refuse silencieusement (il attend un `Hello`, reçoit autre
//! chose, et referme la connexion sans réponse).

use crate::discovery::{ControlRequest, ControlResponse};
use crate::hmac_auth::InitiatorHandshake;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Se connecte à `address`, s'authentifie avec `secret`, envoie `req`, et
/// renvoie la réponse du pair.
pub async fn authenticated_request(
    address: &str,
    secret: &[u8],
    req: &ControlRequest,
) -> Result<ControlResponse, String> {
    let mut stream = authenticated_stream(address, secret).await?;
    write_json_line(&mut stream, req).await?;
    read_json_line(&mut stream).await
}

/// Une connexion authentifiée, prête à porter une requête — pour les
/// échanges qui ne tiennent pas en une ligne, comme la copie d'un modèle.
pub async fn authenticated_stream(address: &str, secret: &[u8]) -> Result<TcpStream, String> {
    let mut stream = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(address))
        .await
        .map_err(|_| format!("{address} ne répond pas (délai dépassé)"))?
        .map_err(|e| format!("connexion à {address} impossible : {e}"))?;

    let (initiateur, hello) = InitiatorHandshake::start(secret);
    write_json_line(&mut stream, &hello).await?;
    let ack = read_json_line(&mut stream).await?;
    let confirm = initiateur.receive_ack(&ack)?;
    write_json_line(&mut stream, &confirm).await?;
    Ok(stream)
}

pub async fn write_json_line<T: serde::Serialize>(
    stream: &mut TcpStream,
    value: &T,
) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await.map_err(|e| e.to_string())
}

/// Lit une ligne JSON octet par octet : rien n'est lu au-delà du saut de
/// ligne, si bien que des octets bruts qui suivent sur la même connexion
/// restent entiers pour l'appelant.
pub async fn read_json_line<T: serde::de::DeserializeOwned>(
    stream: &mut TcpStream,
) -> Result<T, String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut byte))
            .await
            .map_err(|_| "délai dépassé en attendant le pair".to_string())?
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connexion fermée par le pair".to_string());
        }
        if byte[0] == b'\n' {
            break;
        }
        buf.push(byte[0]);
        if buf.len() > 16 * 1024 * 1024 {
            return Err("message de contrôle démesuré".to_string());
        }
    }
    let text = String::from_utf8(buf).map_err(|e| e.to_string())?;
    serde_json::from_str(text.trim()).map_err(|e| format!("message illisible : {e}"))
}
