//! Trouver et installer `llama-server` et `ggml-rpc-server`.
//!
//! Trois emplacements, dans cet ordre : le dossier privé que cette extension
//! gère elle-même, un dossier `bin/llama/` géré par l'application hôte (le
//! même moteur que le runtime intégré, réutilisé plutôt que dupliqué quand il
//! est déjà là), puis le chemin du système. Le premier qui contient le
//! binaire gagne — jamais un mélange des trois.

use crate::backend::{self, Backend};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

fn exe_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

/// Cherche un binaire par son nom (`llama-server`, `ggml-rpc-server`) dans
/// les emplacements connus, puis sur le chemin du système.
pub fn find_binary(name: &str) -> Option<PathBuf> {
    let exe = exe_name(name);

    let prive = crate::vendor_dir().join(&exe);
    if prive.is_file() {
        return Some(prive);
    }

    // Dossier géré par l'application hôte pour son propre runtime llama.cpp
    // — quand il est là, il évite un second téléchargement de plusieurs
    // centaines de mégaoctets pour le même programme.
    if let Some(data_dir) = std::env::var_os("LOCARYN_DATA_DIR").map(PathBuf::from) {
        let partage = data_dir.join("bin").join("llama").join(&exe);
        if partage.is_file() {
            return Some(partage);
        }
    }

    which(&exe)
}

fn which(exe: &str) -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let full = dir.join(exe);
        if full.is_file() {
            return Some(full);
        }
    }
    None
}

/// Où en est un téléchargement : le fichier en cours et les octets reçus.
pub type Progress = dyn Fn(&str, u64) + Send + Sync;

/// Installe la variante `backend` de llama.cpp dans le dossier privé de
/// l'extension, à la place de celle qui y était.
///
/// L'installation se fait dans un dossier voisin, vérifiée, puis échangée avec
/// l'ancien : un téléchargement interrompu ou une archive incomplète laisse la
/// variante précédente intacte. Deux variantes **mélangées** dans un même
/// dossier chargeraient la même carte deux fois (CUDA et Vulkan), d'où
/// l'échange entier plutôt qu'un dépôt par-dessus.
pub async fn install_backend(backend: Backend, progress: &Progress) -> Result<(), String> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    let fichiers = backend::assets(backend, os, arch).ok_or_else(|| {
        format!(
            "llama.cpp ne publie pas de variante « {} » pour {os}/{arch}",
            backend.label()
        )
    })?;

    let final_dir = crate::vendor_dir();
    let staging = final_dir.with_file_name("llama.installation");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;

    let resultat = installer_dans(&staging, backend, &fichiers, progress).await;
    if let Err(e) = resultat {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    if final_dir.exists() {
        std::fs::remove_dir_all(&final_dir).map_err(|e| {
            let _ = std::fs::remove_dir_all(&staging);
            format!(
                "impossible de remplacer les binaires actuels ({e}) — arrêtez le partage de la                  carte (cluster_worker_stop) puis réessayez"
            )
        })?;
    }
    std::fs::rename(&staging, &final_dir).map_err(|e| e.to_string())
}

async fn installer_dans(
    dest: &Path,
    backend: Backend,
    fichiers: &[String],
    progress: &Progress,
) -> Result<(), String> {
    for fichier in fichiers {
        let extension = if fichier.ends_with(".zip") {
            "zip"
        } else {
            "tar.gz"
        };
        let temp = dest.join(format!("telechargement.{extension}"));
        telecharger(&backend::asset_url(fichier), &temp, fichier, progress).await?;
        let resultat = if extension == "zip" {
            extraire_zip(&temp, dest)
        } else {
            extraire_tar_gz(&temp, dest).await
        };
        let _ = std::fs::remove_file(&temp);
        resultat?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for nom in ["llama-server", "ggml-rpc-server"] {
            let chemin = dest.join(nom);
            if chemin.is_file() {
                let _ = std::fs::set_permissions(&chemin, std::fs::Permissions::from_mode(0o755));
            }
        }
    }

    // Les deux programmes servent : `llama-server` pour répondre, `ggml-rpc-server`
    // pour prêter sa carte. Une archive qui n'en a qu'un est refusée ici, avant
    // de remplacer quoi que ce soit.
    for nom in ["llama-server", "ggml-rpc-server"] {
        if !dest.join(exe_name(nom)).is_file() {
            return Err(format!(
                "l'archive « {} » ne contenait pas {nom}",
                backend.label()
            ));
        }
    }
    backend::write_marker(dest, backend).map_err(|e| e.to_string())
}

/// Télécharge avec `curl`, présent sur Windows 10+, Linux et macOS : ce paquet
/// n'embarque volontairement aucune pile TLS (voir `Cargo.toml`), et tout ce
/// qu'il appelle par HTTP est en boucle locale — sauf ce téléchargement.
async fn telecharger(url: &str, dest: &Path, nom: &str, progress: &Progress) -> Result<(), String> {
    let mut cmd = Command::new("curl");
    cmd.args([
        "--fail",
        "--location",
        "--silent",
        "--show-error",
        "--retry",
        "3",
        "--connect-timeout",
        "20",
        "--output",
    ])
    .arg(dest)
    .arg(url)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    hide_console(&mut cmd);
    let mut enfant = cmd
        .spawn()
        .map_err(|e| format!("« curl » introuvable ({e}) : il sert à télécharger llama.cpp"))?;

    loop {
        tokio::select! {
            statut = enfant.wait() => {
                let statut = statut.map_err(|e| e.to_string())?;
                if statut.success() {
                    progress(nom, std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0));
                    return Ok(());
                }
                let mut erreur = String::new();
                if let Some(mut flux) = enfant.stderr.take() {
                    let _ = flux.read_to_string(&mut erreur).await;
                }
                return Err(format!(
                    "téléchargement de {nom} impossible ({statut}) : {}",
                    erreur.trim()
                ));
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(400)) => {
                progress(nom, std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0));
            }
        }
    }
}

fn extraire_zip(archive: &Path, dest: &Path) -> Result<(), String> {
    let fichier = std::fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(fichier).map_err(|e| format!("archive illisible : {e}"))?;
    for i in 0..zip.len() {
        let mut entree = zip.by_index(i).map_err(|e| e.to_string())?;
        if entree.is_dir() {
            continue;
        }
        let Some(nom) = entree
            .enclosed_name()
            .and_then(|p| p.file_name().map(|n| n.to_os_string()))
        else {
            continue;
        };
        let mut sortie = std::fs::File::create(dest.join(&nom)).map_err(|e| e.to_string())?;
        std::io::copy(&mut entree, &mut sortie).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// `.tar.gz` n'a pas de dépendance dans ce paquet : `tar` est présent sur
/// tout Linux et tout macOS, et c'est la seule plateforme où cette extension
/// télécharge une archive de ce format.
async fn extraire_tar_gz(archive: &Path, dest: &Path) -> Result<(), String> {
    let statut = Command::new("tar")
        .args(["xzf"])
        .arg(archive)
        .args(["-C"])
        .arg(dest)
        .args(["--strip-components=0"])
        .status()
        .await
        .map_err(|e| format!("« tar » introuvable : {e}"))?;
    if !statut.success() {
        return Err(format!("« tar » a échoué ({statut})"));
    }
    // `tar` conserve les sous-dossiers de l'archive (`llama-b11003/…`) ; on
    // met à plat comme pour `.zip`, pour que `find_binary` n'ait qu'un seul
    // niveau à chercher.
    aplatir(dest)
}

fn aplatir(dest: &Path) -> Result<(), String> {
    fn visiter(dir: &Path, racine: &Path) -> Result<(), String> {
        let entrees = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
        for entree in entrees.flatten() {
            let chemin = entree.path();
            if chemin.is_dir() {
                visiter(&chemin, racine)?;
                let _ = std::fs::remove_dir(&chemin);
            } else if chemin.parent() != Some(racine) {
                if let Some(nom) = chemin.file_name() {
                    let cible = racine.join(nom);
                    if !cible.exists() {
                        let _ = std::fs::rename(&chemin, &cible);
                    }
                }
            }
        }
        Ok(())
    }
    visiter(dest, dest)
}

/// Pas de fenêtre de console qui clignote sous Windows.
pub fn hide_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        // `tokio::process::Command` porte sa propre `creation_flags` sous
        // Windows : le trait de la bibliothèque standard n'a pas à être
        // importé.
        cmd.creation_flags(0x0800_0008);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}
