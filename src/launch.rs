//! Trouver et installer `llama-server` et `ggml-rpc-server`.
//!
//! Trois emplacements, dans cet ordre : le dossier privé que cette extension
//! gère elle-même, un dossier `bin/llama/` géré par l'application hôte (le
//! même moteur que le runtime intégré, réutilisé plutôt que dupliqué quand il
//! est déjà là), puis le chemin du système. Le premier qui contient le
//! binaire gagne — jamais un mélange des trois.

use std::path::{Path, PathBuf};
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

/// L'archive à récupérer pour cette machine, si l'application en connaît une.
///
/// Même version que le runtime géré par l'hôte (`b11003`), pour que les deux
/// composants restent compatibles s'ils venaient à coexister. La variante
/// choisie est Vulkan — elle calcule sur GPU NVIDIA, AMD et Intel sans
/// exiger un jeu d'outils propre à un fabricant ; une machine avec son propre
/// llama.cpp compilé pour CUDA ou ROCm est trouvée avant d'en arriver là
/// (voir [`find_binary`]).
///
/// L'installateur natif de l'application demandait, pour Linux, une archive
/// `.zip` qui n'existe plus sous ce nom — seule une `.tar.gz` est publiée ;
/// corrigé côté hôte le 16/09/2026 (`services/provider-supervisor`). Cette
/// fonction utilise la bonne extension par plateforme depuis le début, ce
/// qui l'avait rendue indépendante du bogue.
pub fn release_url() -> Option<&'static str> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    match (os, arch) {
        ("windows", _) => Some(
            "https://github.com/ggml-org/llama.cpp/releases/download/b11003/llama-b11003-bin-win-vulkan-x64.zip",
        ),
        ("linux", "x86_64") => Some(
            "https://github.com/ggml-org/llama.cpp/releases/download/b11003/llama-b11003-bin-ubuntu-vulkan-x64.tar.gz",
        ),
        ("linux", "aarch64") => Some(
            "https://github.com/ggml-org/llama.cpp/releases/download/b11003/llama-b11003-bin-ubuntu-vulkan-arm64.tar.gz",
        ),
        ("macos", "aarch64") => Some(
            "https://github.com/ggml-org/llama.cpp/releases/download/b11003/llama-b11003-bin-macos-arm64.tar.gz",
        ),
        ("macos", "x86_64") => Some(
            "https://github.com/ggml-org/llama.cpp/releases/download/b11003/llama-b11003-bin-macos-x64.tar.gz",
        ),
        _ => None,
    }
}

/// Télécharge et pose l'archive dans le dossier privé de l'extension, à
/// plat — les deux binaires qui nous intéressent et leurs bibliothèques
/// partagées côte à côte, quel que soit le sous-dossier où l'archive les
/// range à l'origine.
pub async fn install(http: &reqwest::Client) -> Result<(), String> {
    let url = release_url().ok_or_else(|| {
        format!(
            "aucune archive connue pour {}/{} — installez llama.cpp vous-même et assurez-vous \
             que « llama-server » et « ggml-rpc-server » sont sur le chemin",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let dest = crate::vendor_dir();
    std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;

    let reponse = http
        .get(url)
        .send()
        .await
        .map_err(|e| format!("téléchargement de {url} impossible : {e}"))?;
    if !reponse.status().is_success() {
        return Err(format!("{url} a répondu {}", reponse.status()));
    }
    let corps = reponse
        .bytes()
        .await
        .map_err(|e| format!("téléchargement interrompu : {e}"))?;

    let temp = dest.join(format!(
        "archive.{}",
        if url.ends_with(".zip") {
            "zip"
        } else {
            "tar.gz"
        }
    ));
    std::fs::write(&temp, &corps).map_err(|e| e.to_string())?;

    let resultat = if url.ends_with(".zip") {
        extraire_zip(&temp, &dest)
    } else {
        extraire_tar_gz(&temp, &dest).await
    };
    let _ = std::fs::remove_file(&temp);
    resultat?;

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

    if find_binary("llama-server").is_none() {
        return Err(format!(
            "l'archive a été dépliée dans {} mais ne contenait pas llama-server",
            dest.display()
        ));
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// L'extension de l'archive doit correspondre à ce que la plateforme
    /// publie réellement : Windows en `.zip`, tout le reste en `.tar.gz`.
    /// C'est précisément l'inverse de ce que demande l'installateur natif
    /// pour Linux aujourd'hui (URL `.zip` vérifiée en 404 — seule la
    /// `.tar.gz` existe) ; ce test fige la bonne réponse pour cette
    /// extension.
    #[test]
    fn l_extension_de_l_archive_correspond_a_ce_que_la_plateforme_publie() {
        let Some(url) = release_url() else {
            return; // plateforme non prise en charge : rien à vérifier ici
        };
        let attendu_zip = std::env::consts::OS == "windows";
        assert_eq!(
            url.ends_with(".zip"),
            attendu_zip,
            "extension incohérente pour {url}"
        );
    }
}
