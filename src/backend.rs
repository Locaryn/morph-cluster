//! Choisir la variante de llama.cpp qui convient à la machine.
//!
//! llama.cpp publie un binaire par matériel : CUDA pour les cartes NVIDIA,
//! Vulkan pour à peu près tout le reste, une version pour les processeurs ARM,
//! OpenCL pour le GPU Adreno de Snapdragon, OpenVINO pour l'Intel (dont le
//! NPU). Prendre toujours Vulkan marche partout mais laisse de la vitesse sur
//! la table : ce module devine le meilleur choix **sans jamais promettre plus
//! que ce que la machine montre** — un pilote trop vieux pour le CUDA proposé
//! retombe sur Vulkan plutôt que d'échouer au chargement.
//!
//! La détection lit l'état de la machine ; le choix ([`choose`]) et les noms
//! d'archives ([`assets`]) sont des fonctions pures, testées sans réseau ni
//! matériel.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// La version de llama.cpp que ce module sait télécharger. Même build que le
/// runtime géré par l'hôte, pour que les deux restent compatibles.
pub const BUILD: &str = "b11003";

const BASE_URL: &str = "https://github.com/ggml-org/llama.cpp/releases/download";
const MARQUEUR: &str = "backend.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// Cartes NVIDIA, pilote annonçant CUDA 12.
    Cuda12,
    /// Cartes NVIDIA, pilote annonçant CUDA 13.
    Cuda13,
    /// NVIDIA, AMD, Intel : marche partout où un pilote Vulkan existe.
    Vulkan,
    /// Sans accélérateur : le processeur seul.
    Cpu,
    /// Apple Silicon et Mac Intel (l'archive macOS embarque Metal).
    Metal,
    /// Intel : processeur, GPU et NPU. Le NPU est expérimental — voir
    /// [`Backend::note`].
    OpenVino,
    /// GPU Adreno des PC Windows sur Snapdragon.
    Adreno,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::Cuda12 => "cuda12",
            Backend::Cuda13 => "cuda13",
            Backend::Vulkan => "vulkan",
            Backend::Cpu => "cpu",
            Backend::Metal => "metal",
            Backend::OpenVino => "openvino",
            Backend::Adreno => "adreno",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Backend::Cuda12 => "CUDA 12 (NVIDIA)",
            Backend::Cuda13 => "CUDA 13 (NVIDIA)",
            Backend::Vulkan => "Vulkan (NVIDIA, AMD, Intel)",
            Backend::Cpu => "Processeur seul",
            Backend::Metal => "Metal (Apple)",
            Backend::OpenVino => "OpenVINO (Intel : CPU, GPU, NPU)",
            Backend::Adreno => "OpenCL Adreno (Snapdragon)",
        }
    }

    /// Reconnaît le nom donné par une personne ou une variable d'environnement.
    /// `cuda` seul ne dit pas quelle version : c'est le pilote qui tranche, voir
    /// [`choose`].
    pub fn from_name(nom: &str) -> Option<Backend> {
        match nom.trim().to_ascii_lowercase().as_str() {
            "cuda12" | "cuda-12" => Some(Backend::Cuda12),
            "cuda13" | "cuda-13" => Some(Backend::Cuda13),
            "vulkan" => Some(Backend::Vulkan),
            "cpu" => Some(Backend::Cpu),
            "metal" => Some(Backend::Metal),
            "openvino" => Some(Backend::OpenVino),
            "adreno" | "opencl" => Some(Backend::Adreno),
            _ => None,
        }
    }

    /// Ce qu'il faut savoir avant de choisir ce backend, quand il y a quelque
    /// chose à savoir.
    pub fn note(self) -> Option<&'static str> {
        match self {
            Backend::OpenVino => Some(
                "Le NPU Intel est expérimental dans llama.cpp : mode sans état, quantification \
                 Q4_0 surtout, validé sur Core Ultra série 1 et 2. À réserver à un essai.",
            ),
            Backend::Cuda12 | Backend::Cuda13 => Some(
                "Le paquet CUDA est bien plus gros que Vulkan (il embarque les bibliothèques \
                 d'exécution CUDA).",
            ),
            _ => None,
        }
    }
}

/// Ce que la machine montre, lu une fois.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hardware {
    pub os: String,
    pub arch: String,
    /// La version de CUDA que le **pilote** annonce (`nvidia-smi`), pas celle
    /// d'un éventuel kit installé : un binaire CUDA ne tourne que si le pilote
    /// est au moins aussi récent que lui.
    pub cuda: Option<(u32, u32)>,
    /// Un GPU Adreno (Windows sur Snapdragon).
    pub adreno: bool,
    /// Un NPU Intel est présent. Détecté pour être signalé, jamais choisi
    /// tout seul : le support est expérimental.
    pub intel_npu: bool,
}

impl Hardware {
    /// La machine sur laquelle on tourne, sans sonde : ce que le compilateur
    /// sait déjà.
    pub fn bare() -> Self {
        Hardware {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            cuda: None,
            adreno: false,
            intel_npu: false,
        }
    }
}

/// Lit la version de CUDA dans l'en-tête de `nvidia-smi`. Deux formes selon
/// l'âge du pilote : « CUDA Version: 12.4 » et « CUDA UMD Version: 13.4 ».
pub fn parse_cuda_version(sortie: &str) -> Option<(u32, u32)> {
    let position = sortie.find("CUDA")?;
    let reste = &sortie[position..];
    let apres = reste.split("Version:").nth(1)?;
    let version = apres.split_whitespace().next()?;
    let (majeure, mineure) = version.split_once('.')?;
    Some((majeure.parse().ok()?, mineure.parse().ok()?))
}

fn arch_archive(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" => Some("x64"),
        "aarch64" => Some("arm64"),
        _ => None,
    }
}

/// Les versions minimales de CUDA des archives publiées, par système : Windows
/// propose CUDA 12.4 et 13.4, Linux 12.8 et 13.3. Sous ARM64, seul CUDA 13
/// existe.
fn cuda_minimums(os: &str) -> ((u32, u32), (u32, u32)) {
    if os == "windows" {
        ((13, 4), (12, 4))
    } else {
        ((13, 3), (12, 8))
    }
}

fn cuda_variant(os: &str, arch: &str, pilote: (u32, u32)) -> Option<Backend> {
    let (min13, min12) = cuda_minimums(os);
    if pilote >= min13 {
        return Some(Backend::Cuda13);
    }
    if arch == "x86_64" && pilote >= min12 {
        return Some(Backend::Cuda12);
    }
    None
}

/// Les archives à télécharger pour ce backend sur cette machine, ou `None` si
/// llama.cpp n'en publie pas pour cette combinaison. Le premier fichier porte
/// les programmes ; un éventuel second (`cudart`) porte les bibliothèques
/// CUDA, à déplier dans le même dossier.
pub fn assets(backend: Backend, os: &str, arch: &str) -> Option<Vec<String>> {
    let a = arch_archive(arch)?;
    let b = BUILD;
    let fichiers: Vec<String> = match (backend, os) {
        (Backend::Metal, "macos") => vec![format!("llama-{b}-bin-macos-{a}.tar.gz")],
        (Backend::Cpu, "macos") => vec![format!("llama-{b}-bin-macos-{a}.tar.gz")],

        (Backend::Vulkan, "windows") if a == "x64" => {
            vec![format!("llama-{b}-bin-win-vulkan-x64.zip")]
        }
        (Backend::Vulkan, "linux") => vec![format!("llama-{b}-bin-ubuntu-vulkan-{a}.tar.gz")],

        (Backend::Cpu, "windows") => vec![format!("llama-{b}-bin-win-cpu-{a}.zip")],
        (Backend::Cpu, "linux") => vec![format!("llama-{b}-bin-ubuntu-{a}.tar.gz")],

        (Backend::Cuda12, "windows") if a == "x64" => vec![
            format!("llama-{b}-bin-win-cuda-12.4-x64.zip"),
            "cudart-llama-bin-win-cuda-12.4-x64.zip".to_string(),
        ],
        (Backend::Cuda13, "windows") => vec![
            format!("llama-{b}-bin-win-cuda-13.4-{a}.zip"),
            format!("cudart-llama-bin-win-cuda-13.4-{a}.zip"),
        ],
        (Backend::Cuda12, "linux") if a == "x64" => vec![
            format!("llama-{b}-bin-ubuntu-cuda-12.8-x64.tar.gz"),
            format!("cudart-llama-{b}-bin-ubuntu-cuda-12.8-x64.tar.gz"),
        ],
        (Backend::Cuda13, "linux") => vec![
            format!("llama-{b}-bin-ubuntu-cuda-13.3-{a}.tar.gz"),
            format!("cudart-llama-{b}-bin-ubuntu-cuda-13.3-{a}.tar.gz"),
        ],

        (Backend::Adreno, "windows") if a == "arm64" => {
            vec![format!("llama-{b}-bin-win-opencl-adreno-arm64.zip")]
        }
        (Backend::OpenVino, "windows") if a == "x64" => {
            vec![format!("llama-{b}-bin-win-openvino-2026.3.1-x64.zip")]
        }
        (Backend::OpenVino, "linux") if a == "x64" => {
            vec![format!("llama-{b}-bin-ubuntu-openvino-2026.3.1-x64.tar.gz")]
        }
        _ => return None,
    };
    Some(fichiers)
}

/// L'adresse d'une archive de la version épinglée.
pub fn asset_url(fichier: &str) -> String {
    format!("{BASE_URL}/{BUILD}/{fichier}")
}

/// La taille approximative du téléchargement, en mégaoctets, pour que la
/// personne sache ce qu'elle accepte. Mesurée sur la publication `b11003` ;
/// `None` là où on ne l'a pas mesurée.
pub fn approx_download_mb(backend: Backend, os: &str, arch: &str) -> Option<u32> {
    match (backend, os, arch) {
        (Backend::Metal | Backend::Cpu, "macos", _) => Some(10),
        (Backend::Vulkan, "windows", "x86_64") => Some(30),
        (Backend::Cpu, "windows", "x86_64") => Some(17),
        (Backend::Cuda12, "windows", "x86_64") => Some(615),
        (Backend::Cuda13, "windows", "x86_64") => Some(546),
        (Backend::OpenVino, "windows", "x86_64") => Some(76),
        (Backend::Cpu, "windows", "aarch64") => Some(11),
        (Backend::Adreno, "windows", "aarch64") => Some(12),
        (Backend::Cuda13, "windows", "aarch64") => Some(282),
        (Backend::Vulkan, "linux", "x86_64") => Some(28),
        (Backend::Cpu, "linux", "x86_64") => Some(16),
        (Backend::Cuda12, "linux", "x86_64") => Some(726),
        (Backend::Cuda13, "linux", "x86_64") => Some(533),
        (Backend::OpenVino, "linux", "x86_64") => Some(96),
        (Backend::Vulkan, "linux", "aarch64") => Some(23),
        (Backend::Cpu, "linux", "aarch64") => Some(12),
        (Backend::Cuda13, "linux", "aarch64") => Some(632),
        _ => None,
    }
}

/// Le meilleur backend pour cette machine, ou celui que la personne impose
/// (`forced`) quand il existe pour ce système.
///
/// L'ordre, quand rien n'est imposé : CUDA si le pilote NVIDIA l'autorise ;
/// sur Windows ARM, Adreno si le GPU est là, sinon le processeur ; sur Mac,
/// Metal ; sinon Vulkan. OpenVINO n'est **jamais** choisi tout seul : son NPU
/// est expérimental.
pub fn choose(hardware: &Hardware, forced: Option<Backend>) -> Backend {
    let (os, arch) = (hardware.os.as_str(), hardware.arch.as_str());

    if let Some(voulu) = forced {
        if assets(voulu, os, arch).is_some() {
            return voulu;
        }
    }

    if os == "macos" {
        return Backend::Metal;
    }

    if let Some(pilote) = hardware.cuda {
        if let Some(cuda) = cuda_variant(os, arch, pilote) {
            return cuda;
        }
    }

    if os == "windows" && arch == "aarch64" {
        return if hardware.adreno {
            Backend::Adreno
        } else {
            Backend::Cpu
        };
    }

    if assets(Backend::Vulkan, os, arch).is_some() {
        return Backend::Vulkan;
    }
    Backend::Cpu
}

/// Lit la machine : pilote NVIDIA, GPU Adreno, NPU Intel. Chaque sonde qui
/// échoue laisse simplement son champ vide — une machine sans `nvidia-smi`
/// n'a pas de CUDA, ce n'est pas une erreur.
pub async fn detect() -> Hardware {
    let mut hardware = Hardware::bare();

    let mut nvidia = tokio::process::Command::new("nvidia-smi");
    crate::launch::hide_console(&mut nvidia);
    if let Ok(sortie) = nvidia.output().await {
        if sortie.status.success() {
            hardware.cuda = parse_cuda_version(&String::from_utf8_lossy(&sortie.stdout));
        }
    }

    if hardware.os == "windows" {
        let noms = powershell(
            "Get-CimInstance Win32_PnPEntity | Where-Object { $_.Name -match 'NPU|AI Boost|Adreno' } | ForEach-Object { $_.Name }",
        )
        .await;
        let minuscules = noms.to_ascii_lowercase();
        hardware.adreno = hardware.arch == "aarch64" && minuscules.contains("adreno");
        hardware.intel_npu = hardware.arch == "x86_64"
            && (minuscules.contains("ai boost") || minuscules.contains("intel(r) npu"));
    } else if hardware.os == "linux" {
        hardware.intel_npu = Path::new("/dev/accel/accel0").exists() && hardware.arch == "x86_64";
    }

    hardware
}

async fn powershell(commande: &str) -> String {
    let mut cmd = tokio::process::Command::new("powershell");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", commande]);
    crate::launch::hide_console(&mut cmd);
    match cmd.output().await {
        Ok(sortie) if sortie.status.success() => {
            String::from_utf8_lossy(&sortie.stdout).to_string()
        }
        _ => String::new(),
    }
}

/// Traduit un nom donné par une personne (ou l'outil) en backend pour cette
/// machine. `auto` laisse [`choose`] décider ; `cuda` seul laisse le pilote
/// choisir entre 12 et 13, et échoue s'il n'y a pas de pilote NVIDIA
/// utilisable — installer CUDA sans carte NVIDIA donnerait un binaire qui ne
/// charge pas.
pub fn resolve(nom: &str, hardware: &Hardware) -> Result<Backend, String> {
    let nom = nom.trim().to_ascii_lowercase();
    if nom.is_empty() || nom == "auto" {
        return Ok(choose(hardware, None));
    }
    let backend = if nom == "cuda" {
        hardware
            .cuda
            .and_then(|v| cuda_variant(&hardware.os, &hardware.arch, v))
            .ok_or_else(|| {
                "aucun pilote NVIDIA compatible avec les archives CUDA de llama.cpp".to_string()
            })?
    } else {
        Backend::from_name(&nom).ok_or_else(|| {
            format!("backend inconnu : « {nom} » (auto, cuda, vulkan, cpu, openvino, adreno)")
        })?
    };
    if assets(backend, &hardware.os, &hardware.arch).is_none() {
        return Err(format!(
            "llama.cpp ne publie pas « {} » pour {}/{}",
            backend.label(),
            hardware.os,
            hardware.arch
        ));
    }
    Ok(backend)
}

/// Le choix imposé par `LOCARYN_CLUSTER_BACKEND`, s'il y en a un et qu'il est
/// valable ici ; sinon, le choix automatique.
pub fn recommended(hardware: &Hardware) -> Backend {
    let force = std::env::var("LOCARYN_CLUSTER_BACKEND")
        .ok()
        .and_then(|nom| resolve(&nom, hardware).ok());
    choose(hardware, force)
}

/// Ce qui est écrit à côté des binaires installés, pour savoir plus tard de
/// quelle variante ils viennent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    pub backend: Backend,
    pub build: String,
}

pub fn write_marker(dossier: &Path, backend: Backend) -> std::io::Result<()> {
    let marqueur = Installed {
        backend,
        build: BUILD.to_string(),
    };
    let json = serde_json::to_string_pretty(&marqueur).unwrap_or_else(|_| "{}".to_string());
    std::fs::write(dossier.join(MARQUEUR), json)
}

pub fn read_marker(dossier: &Path) -> Option<Installed> {
    let texte = std::fs::read_to_string(dossier.join(MARQUEUR)).ok()?;
    serde_json::from_str(&texte).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(os: &str, arch: &str) -> Hardware {
        Hardware {
            os: os.to_string(),
            arch: arch.to_string(),
            cuda: None,
            adreno: false,
            intel_npu: false,
        }
    }

    /// L'en-tête réel de `nvidia-smi` sur la machine de développement (pilote
    /// 616.56) : la forme récente dit « CUDA UMD Version ».
    #[test]
    fn la_version_cuda_se_lit_dans_l_en_tete_recent() {
        let sortie = "| NVIDIA-SMI 616.56                 KMD Version: 616.56        CUDA UMD Version: 13.4     |";
        assert_eq!(parse_cuda_version(sortie), Some((13, 4)));
    }

    #[test]
    fn la_version_cuda_se_lit_dans_l_en_tete_ancien() {
        let sortie = "| NVIDIA-SMI 551.61                 Driver Version: 551.61         CUDA Version: 12.4     |";
        assert_eq!(parse_cuda_version(sortie), Some((12, 4)));
        assert_eq!(parse_cuda_version("pas de nvidia ici"), None);
    }

    #[test]
    fn un_pilote_recent_prend_cuda_13_et_un_ancien_cuda_12() {
        let mut m = machine("windows", "x86_64");
        m.cuda = Some((13, 4));
        assert_eq!(choose(&m, None), Backend::Cuda13);
        m.cuda = Some((12, 6));
        assert_eq!(choose(&m, None), Backend::Cuda12);
    }

    /// Un pilote trop vieux pour tout CUDA publié retombe sur Vulkan : mieux
    /// vaut une carte un peu moins bien exploitée qu'un binaire qui ne charge
    /// pas.
    #[test]
    fn un_pilote_trop_vieux_retombe_sur_vulkan() {
        let mut m = machine("windows", "x86_64");
        m.cuda = Some((11, 8));
        assert_eq!(choose(&m, None), Backend::Vulkan);
        m.cuda = Some((13, 0));
        // 13.0 ne suffit pas pour l'archive 13.4 ; 12.4 tourne sur ce pilote.
        assert_eq!(choose(&m, None), Backend::Cuda12);
    }

    #[test]
    fn sans_nvidia_c_est_vulkan_sur_pc_x86() {
        assert_eq!(choose(&machine("windows", "x86_64"), None), Backend::Vulkan);
        assert_eq!(choose(&machine("linux", "x86_64"), None), Backend::Vulkan);
        assert_eq!(choose(&machine("linux", "aarch64"), None), Backend::Vulkan);
    }

    /// Le bogue que ce module corrige : un PC Windows sur Snapdragon recevait
    /// l'archive x64, qui n'existe pas en Vulkan pour ARM.
    #[test]
    fn windows_arm_ne_recoit_jamais_l_archive_x64() {
        let sans_gpu = machine("windows", "aarch64");
        assert_eq!(choose(&sans_gpu, None), Backend::Cpu);
        let mut adreno = sans_gpu.clone();
        adreno.adreno = true;
        assert_eq!(choose(&adreno, None), Backend::Adreno);
        let fichiers = assets(choose(&adreno, None), "windows", "aarch64").unwrap();
        assert!(fichiers[0].contains("arm64"), "{fichiers:?}");
    }

    #[test]
    fn un_mac_prend_metal() {
        assert_eq!(choose(&machine("macos", "aarch64"), None), Backend::Metal);
        assert_eq!(choose(&machine("macos", "x86_64"), None), Backend::Metal);
    }

    /// Le NPU Intel est expérimental : présent, il est signalé, jamais choisi
    /// sans que la personne le demande.
    #[test]
    fn le_npu_intel_n_est_jamais_choisi_tout_seul() {
        let mut m = machine("windows", "x86_64");
        m.intel_npu = true;
        assert_eq!(choose(&m, None), Backend::Vulkan);
        assert_eq!(choose(&m, Some(Backend::OpenVino)), Backend::OpenVino);
    }

    #[test]
    fn un_choix_impossible_sur_ce_systeme_est_ignore() {
        // Adreno n'existe que pour Windows ARM : imposé ailleurs, on retombe
        // sur le choix automatique.
        let m = machine("linux", "x86_64");
        assert_eq!(choose(&m, Some(Backend::Adreno)), Backend::Vulkan);
    }

    #[test]
    fn resoudre_un_nom_tient_compte_de_la_machine() {
        let mut m = machine("windows", "x86_64");
        assert_eq!(resolve("auto", &m).unwrap(), Backend::Vulkan);
        assert!(
            resolve("cuda", &m).is_err(),
            "pas de pilote NVIDIA : CUDA ne peut pas être installé"
        );
        m.cuda = Some((13, 4));
        assert_eq!(resolve("cuda", &m).unwrap(), Backend::Cuda13);
        assert_eq!(resolve("vulkan", &m).unwrap(), Backend::Vulkan);
        assert!(
            resolve("adreno", &m).is_err(),
            "Adreno n'existe pas sur x86"
        );
        assert!(resolve("nimporte", &m).unwrap_err().contains("inconnu"));
    }

    #[test]
    fn cuda_embarque_ses_bibliotheques_d_execution() {
        let fichiers = assets(Backend::Cuda13, "windows", "x86_64").unwrap();
        assert_eq!(fichiers.len(), 2);
        assert!(fichiers[1].starts_with("cudart-"), "{fichiers:?}");
        assert_eq!(
            assets(Backend::Vulkan, "windows", "x86_64").unwrap().len(),
            1
        );
    }

    #[test]
    fn les_archives_de_chaque_systeme_ont_la_bonne_extension() {
        for backend in [Backend::Vulkan, Backend::Cpu, Backend::Cuda13] {
            for f in assets(backend, "windows", "x86_64").into_iter().flatten() {
                assert!(f.ends_with(".zip"), "{f}");
            }
            for f in assets(backend, "linux", "x86_64").into_iter().flatten() {
                assert!(f.ends_with(".tar.gz"), "{f}");
            }
        }
    }

    #[test]
    fn le_marqueur_dit_de_quelle_variante_viennent_les_binaires() {
        let dossier = std::env::temp_dir().join(format!(
            "locaryn_backend_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dossier).unwrap();
        assert_eq!(read_marker(&dossier), None);
        write_marker(&dossier, Backend::Cuda13).unwrap();
        assert_eq!(read_marker(&dossier).unwrap().backend, Backend::Cuda13);
        let _ = std::fs::remove_dir_all(&dossier);
    }

    /// Vérifie contre GitHub que chaque archive que ce module nomme existe
    /// vraiment dans la publication épinglée : un nom qui dérive (une version
    /// de CUDA changée d'une publication à l'autre) ne se voit autrement qu'au
    /// premier téléchargement d'un utilisateur. Réseau : `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn toutes_les_archives_nommees_existent_dans_la_publication() {
        let sortie = std::process::Command::new("curl")
            .args([
                "-sL",
                &format!("https://api.github.com/repos/ggml-org/llama.cpp/releases/tags/{BUILD}"),
            ])
            .output()
            .expect("curl");
        let publication = String::from_utf8_lossy(&sortie.stdout);
        let mut manquantes = Vec::new();
        for (os, arch) in [
            ("windows", "x86_64"),
            ("windows", "aarch64"),
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("macos", "aarch64"),
            ("macos", "x86_64"),
        ] {
            for backend in [
                Backend::Cuda12,
                Backend::Cuda13,
                Backend::Vulkan,
                Backend::Cpu,
                Backend::Metal,
                Backend::OpenVino,
                Backend::Adreno,
            ] {
                for f in assets(backend, os, arch).into_iter().flatten() {
                    if !publication.contains(&format!("\"{f}\"")) {
                        manquantes.push(f);
                    }
                }
            }
        }
        assert!(
            manquantes.is_empty(),
            "archives introuvables : {manquantes:?}"
        );
    }
}
