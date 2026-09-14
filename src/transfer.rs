//! La copie d'un modèle partagé, du coordinateur vers un poste client.
//!
//! Sur la connexion de contrôle authentifiée : le poste demande un fichier du
//! catalogue à partir d'un octet, le coordinateur répond par un en-tête puis
//! les octets bruts. La copie s'écrit dans un fichier `.part` à côté de sa
//! destination, n'est renommée qu'une fois complète **et** vérifiée contre
//! l'empreinte du catalogue, et reprend là où elle s'était arrêtée.
//!
//! Le canal est authentifié, pas chiffré : sur le réseau local, un modèle
//! partagé n'est pas un secret, mais un fichier altéré en route serait chargé
//! par llama.cpp — d'où la vérification SHA-256 avant de le rendre visible.

use crate::catalog::{self, SharedModel};
use crate::discovery::{ControlRequest, ControlResponse};
use crate::peer_channel;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TAILLE_BLOC: usize = 1024 * 1024;

/// Le fichier temporaire d'une copie en cours.
pub fn part_path(dest: &Path) -> PathBuf {
    let mut nom = dest.as_os_str().to_os_string();
    nom.push(".part");
    PathBuf::from(nom)
}

/// Télécharge `model` depuis le coordinateur jusqu'à `models_dir`.
///
/// `progress` reçoit le nombre d'octets présents sur le disque, reprise
/// comprise, pour que le panneau affiche un pourcentage honnête.
pub async fn fetch_model(
    coordinator: &str,
    secret: &[u8],
    models_dir: &Path,
    model: &SharedModel,
    progress: Arc<AtomicU64>,
) -> Result<PathBuf, String> {
    let expected = model
        .sha256
        .clone()
        .ok_or("empreinte absente du catalogue : le coordinateur la calcule encore")?;
    let dest = catalog::local_path(models_dir, &model.file)
        .ok_or_else(|| format!("nom de fichier refusé : {}", model.file))?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{} : {e}", parent.display()))?;
    }
    let part = part_path(&dest);
    let mut offset = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if offset > model.size_bytes {
        // Plus long que l'original : ce fragment n'est pas une copie de ce
        // fichier-ci. On repart de zéro plutôt que de le compléter.
        let _ = std::fs::remove_file(&part);
        offset = 0;
    }
    progress.store(offset, Ordering::Relaxed);

    if offset < model.size_bytes {
        receive(coordinator, secret, model, &part, offset, &progress).await?;
    }

    let part_hash = part.clone();
    let hash = tokio::task::spawn_blocking(move || catalog::sha256_file(&part_hash))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("lecture de la copie : {e}"))?;
    if !hash.eq_ignore_ascii_case(&expected) {
        let _ = std::fs::remove_file(&part);
        return Err(format!(
            "{} : empreinte différente de celle du coordinateur — copie supprimée, elle sera \
             reprise au prochain passage",
            model.file
        ));
    }
    std::fs::rename(&part, &dest).map_err(|e| format!("{} : {e}", dest.display()))?;
    Ok(dest)
}

async fn receive(
    coordinator: &str,
    secret: &[u8],
    model: &SharedModel,
    part: &Path,
    offset: u64,
    progress: &AtomicU64,
) -> Result<(), String> {
    let mut stream = peer_channel::authenticated_stream(coordinator, secret).await?;
    peer_channel::write_json_line(
        &mut stream,
        &ControlRequest::FetchModel {
            file: model.file.clone(),
            offset,
        },
    )
    .await?;
    match peer_channel::read_json_line::<ControlResponse>(&mut stream).await? {
        ControlResponse::ModelStream {
            size_bytes,
            offset: debut,
            ..
        } if size_bytes == model.size_bytes && debut == offset => {}
        ControlResponse::ModelStream { .. } => {
            return Err(format!(
                "{} a changé sur le coordinateur depuis la lecture du catalogue",
                model.file
            ))
        }
        ControlResponse::Error { message } => return Err(message),
        _ => return Err("réponse inattendue du coordinateur".to_string()),
    }

    let mut out = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(part)
        .await
        .map_err(|e| format!("{} : {e}", part.display()))?;
    let mut recu = offset;
    let mut buf = vec![0u8; TAILLE_BLOC];
    while recu < model.size_bytes {
        let n = tokio::time::timeout(Duration::from_secs(60), stream.read(&mut buf))
            .await
            .map_err(|_| "le coordinateur n'envoie plus rien (60 s)".to_string())?
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("copie interrompue par le coordinateur".to_string());
        }
        let n = n.min((model.size_bytes - recu) as usize);
        out.write_all(&buf[..n])
            .await
            .map_err(|e| format!("écriture sur ce disque : {e}"))?;
        recu += n as u64;
        progress.store(recu, Ordering::Relaxed);
    }
    out.flush().await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Côté coordinateur : envoie un fichier du catalogue à partir de `offset`.
///
/// Le fichier doit figurer au catalogue **avec son empreinte** : un pair
/// authentifié ne peut pas se servir dans la bibliothèque au-delà de ce qui
/// est explicitement partagé.
pub async fn serve_model(
    stream: &mut TcpStream,
    catalog: &catalog::Catalog,
    models_dir: &Path,
    file: &str,
    offset: u64,
) -> Result<(), String> {
    let refus = |message: String| ControlResponse::Error { message };
    let Some(model) = catalog.get(file).filter(|m| m.sha256.is_some()) else {
        let r = refus(format!("« {file} » n'est pas partagé"));
        return peer_channel::write_json_line(stream, &r).await;
    };
    let Some(path) = catalog::local_path(models_dir, file) else {
        let r = refus(format!("nom de fichier refusé : {file}"));
        return peer_channel::write_json_line(stream, &r).await;
    };
    let taille = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if taille != model.size_bytes || offset > taille {
        let r = refus(format!(
            "« {file} » a changé depuis son partage : réactivez-le pour recalculer son empreinte"
        ));
        return peer_channel::write_json_line(stream, &r).await;
    }

    let mut source = tokio::fs::File::open(&path)
        .await
        .map_err(|e| format!("{} : {e}", path.display()))?;
    if offset > 0 {
        use tokio::io::AsyncSeekExt;
        source
            .seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| e.to_string())?;
    }
    peer_channel::write_json_line(
        stream,
        &ControlResponse::ModelStream {
            file: file.to_string(),
            size_bytes: taille,
            offset,
        },
    )
    .await?;
    tokio::io::copy(&mut source, stream)
        .await
        .map_err(|e| format!("envoi interrompu : {e}"))?;
    stream.flush().await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use crate::hmac_auth::ResponderHandshake;
    use tokio::net::TcpListener;

    /// De bout en bout sur la boucle locale : poignée de main, reprise à mi-
    /// fichier, vérification de l'empreinte, renommage.
    #[tokio::test]
    async fn un_modele_se_copie_reprend_et_se_verifie() {
        let racine = std::env::temp_dir().join(format!("cluster-transfer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&racine);
        let source_dir = racine.join("serveur");
        let dest_dir = racine.join("client");
        std::fs::create_dir_all(&source_dir).unwrap();
        let contenu: Vec<u8> = (0..(3 * TAILLE_BLOC + 123))
            .map(|i| (i % 251) as u8)
            .collect();
        std::fs::write(source_dir.join("m.gguf"), &contenu).unwrap();
        let modele = SharedModel {
            file: "m.gguf".into(),
            size_bytes: contenu.len() as u64,
            sha256: Some(catalog::sha256_file(&source_dir.join("m.gguf")).unwrap()),
            modified_unix: 0,
        };
        let catalogue = Catalog {
            models: vec![modele.clone()],
        };

        // Une copie interrompue : les 1000 premiers octets sont déjà là.
        std::fs::create_dir_all(&dest_dir).unwrap();
        std::fs::write(part_path(&dest_dir.join("m.gguf")), &contenu[..1000]).unwrap();

        let secret = b"0123456789abcdef0123456789abcdef".to_vec();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let adresse = listener.local_addr().unwrap().to_string();
        let secret_serveur = secret.clone();
        let serveur = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let hello = peer_channel::read_json_line(&mut stream).await.unwrap();
            let (repondant, ack) = ResponderHandshake::respond(&secret_serveur, &hello);
            peer_channel::write_json_line(&mut stream, &ack)
                .await
                .unwrap();
            let confirm = peer_channel::read_json_line(&mut stream).await.unwrap();
            repondant.receive_confirm(&confirm).unwrap();
            match peer_channel::read_json_line(&mut stream).await.unwrap() {
                ControlRequest::FetchModel { file, offset } => {
                    assert_eq!(offset, 1000);
                    serve_model(&mut stream, &catalogue, &source_dir, &file, offset)
                        .await
                        .unwrap();
                }
                autre => panic!("requête inattendue : {autre:?}"),
            }
        });

        let progres = Arc::new(AtomicU64::new(0));
        let chemin = fetch_model(&adresse, &secret, &dest_dir, &modele, progres.clone())
            .await
            .unwrap();
        serveur.await.unwrap();
        assert_eq!(std::fs::read(&chemin).unwrap(), contenu);
        assert!(!part_path(&chemin).exists());
        assert_eq!(progres.load(Ordering::Relaxed), contenu.len() as u64);
        let _ = std::fs::remove_dir_all(&racine);
    }

    #[tokio::test]
    async fn un_fichier_hors_catalogue_est_refuse() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let adresse = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut s = TcpStream::connect(adresse).await.unwrap();
            peer_channel::read_json_line::<ControlResponse>(&mut s)
                .await
                .unwrap()
        });
        let (mut stream, _) = listener.accept().await.unwrap();
        serve_model(
            &mut stream,
            &Catalog::default(),
            Path::new("."),
            "../secret.gguf",
            0,
        )
        .await
        .unwrap();
        match client.await.unwrap() {
            ControlResponse::Error { message } => assert!(message.contains("pas partagé")),
            autre => panic!("réponse inattendue : {autre:?}"),
        }
    }
}
