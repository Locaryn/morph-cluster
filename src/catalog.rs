//! Les modèles partagés : ce que le coordinateur met à disposition, et ce
//! qu'un poste client doit en garder une copie.
//!
//! Le coordinateur tient le catalogue — une liste de fichiers GGUF de sa
//! bibliothèque, avec leur taille et leur empreinte SHA-256. Chaque poste
//! client qui accepte d'héberger des modèles compare ce catalogue à ce qu'il a
//! déjà, et en déduit quoi télécharger, quoi supprimer, et ce que son quota
//! ne permet pas.
//!
//! Ce module ne touche pas au réseau : la décision est une fonction pure,
//! testée à part de la copie elle-même.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Un modèle que le coordinateur partage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SharedModel {
    /// Chemin relatif à la bibliothèque de poids, séparateurs `/`.
    pub file: String,
    pub size_bytes: u64,
    /// `None` tant que l'empreinte est en calcul : un poste client attend
    /// plutôt que de copier un fichier qu'il ne pourrait pas vérifier.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Date de modification relevée avec l'empreinte : un fichier remplacé
    /// sous le même nom doit être recalculé, pas cru sur parole.
    #[serde(default)]
    pub modified_unix: u64,
}

/// Le catalogue du coordinateur.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(default)]
    pub models: Vec<SharedModel>,
}

impl Catalog {
    pub fn load(dir: &Path) -> Self {
        std::fs::read_to_string(dir.join("shared_models.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let json = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        std::fs::write(dir.join("shared_models.json"), json)
    }

    pub fn get(&self, file: &str) -> Option<&SharedModel> {
        self.models.iter().find(|m| m.file == file)
    }

    pub fn contains(&self, file: &str) -> bool {
        self.get(file).is_some()
    }
}

/// Les copies que ce poste a lui-même téléchargées. Seules celles-ci peuvent
/// être supprimées quand le coordinateur cesse de partager un modèle : un
/// fichier du même nom posé là par la personne n'appartient pas au cluster.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncLedger {
    #[serde(default)]
    pub files: BTreeSet<String>,
}

impl SyncLedger {
    pub fn load(dir: &Path) -> Self {
        std::fs::read_to_string(dir.join("synced.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let json = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        std::fs::write(dir.join("synced.json"), json)
    }
}

/// Un nom de fichier du catalogue est-il sûr à joindre à un dossier local ?
///
/// Le nom vient d'une autre machine. Sans cette vérification, `../../` y
/// écrirait n'importe où sur le disque du poste client, et y lirait n'importe
/// quoi sur celui du coordinateur.
pub fn is_safe_relative(file: &str) -> bool {
    let octets = file.as_bytes();
    let lecteur = octets.len() > 1 && octets[1] == b':' && octets[0].is_ascii_alphabetic();
    !file.is_empty()
        && !lecteur
        && !file.starts_with(['/', '\\'])
        && file.to_ascii_lowercase().ends_with(".gguf")
        && file
            .split(['/', '\\'])
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// Le chemin local d'un fichier du catalogue, ou `None` s'il n'est pas sûr.
pub fn local_path(models_dir: &Path, file: &str) -> Option<PathBuf> {
    if !is_safe_relative(file) {
        return None;
    }
    Some(
        file.split(['/', '\\'])
            .fold(models_dir.to_path_buf(), |p, s| p.join(s)),
    )
}

/// Un fichier GGUF de la bibliothèque.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibraryModel {
    pub file: String,
    pub size_bytes: u64,
    pub modified_unix: u64,
}

/// Les fichiers GGUF de la bibliothèque, sur deux niveaux : la racine, et un
/// dossier par modèle. Les fragments `.part` d'un téléchargement en cours
/// n'en font pas partie.
pub fn scan_library(models_dir: &Path) -> Vec<LibraryModel> {
    let mut out = Vec::new();
    visit(models_dir, models_dir, 0, &mut out);
    out.sort_by(|a, b| a.file.to_lowercase().cmp(&b.file.to_lowercase()));
    out
}

fn visit(root: &Path, dir: &Path, depth: usize, out: &mut Vec<LibraryModel>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            if depth < 1 {
                visit(root, &path, depth + 1, out);
            }
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let file = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("/");
        if !is_safe_relative(&file) {
            continue;
        }
        out.push(LibraryModel {
            file,
            size_bytes: meta.len(),
            modified_unix: modified_unix(&meta),
        });
    }
}

pub fn modified_unix(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// L'empreinte SHA-256 d'un fichier, en hexadécimal. Bloquant : à appeler
/// hors de la boucle asynchrone, un modèle de 16 Go se lit en près d'une
/// minute.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Ce qu'un modèle du catalogue est sur ce poste.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LocalState {
    /// Une copie complète est là.
    Ready,
    /// À télécharger, dans l'ordre du plan.
    Pending,
    /// L'empreinte n'est pas encore calculée sur le coordinateur.
    Preparing,
    /// Le quota ne le permet pas.
    NoRoom { missing_bytes: u64 },
    /// Ce poste n'héberge pas de modèles.
    NotHosted,
}

/// Le plan de synchronisation d'un poste client.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncPlan {
    /// Par fichier du catalogue, dans son ordre.
    pub states: Vec<(String, LocalState)>,
    /// À télécharger, dans l'ordre.
    pub download: Vec<SharedModel>,
    /// Copies à supprimer : téléchargées par ce poste, plus partagées.
    pub remove: Vec<String>,
    /// Place occupée par les copies gardées, en octets.
    pub used_bytes: u64,
}

/// Décide quoi télécharger et quoi supprimer.
///
/// - `local_sizes` : la taille des fichiers déjà présents dans la
///   bibliothèque, par nom de catalogue.
/// - Un fichier présent **à la bonne taille** compte comme prêt, qu'il ait
///   été copié par ce poste ou posé par la personne : le vérifier octet par
///   octet à chaque passage coûterait une minute par modèle.
/// - Seules les copies du registre comptent dans le quota : un modèle que la
///   personne avait déjà n'occupe pas la place qu'elle a accordée au cluster.
pub fn plan_sync(
    catalog: &Catalog,
    local_sizes: &BTreeMap<String, u64>,
    ledger: &SyncLedger,
    hosts_models: bool,
    quota_bytes: u64,
) -> SyncPlan {
    let mut plan = SyncPlan {
        remove: ledger
            .files
            .iter()
            .filter(|f| !hosts_models || !catalog.contains(f))
            .cloned()
            .collect(),
        ..SyncPlan::default()
    };

    plan.used_bytes = catalog
        .models
        .iter()
        .filter(|m| hosts_models && ledger.files.contains(&m.file))
        .filter(|m| local_sizes.get(&m.file) == Some(&m.size_bytes))
        .map(|m| m.size_bytes)
        .sum();
    let mut reserve = plan.used_bytes;

    for model in &catalog.models {
        let present = local_sizes.get(&model.file) == Some(&model.size_bytes);
        let state = if present {
            LocalState::Ready
        } else if !hosts_models {
            LocalState::NotHosted
        } else if model.sha256.is_none() {
            LocalState::Preparing
        } else if reserve + model.size_bytes > quota_bytes {
            LocalState::NoRoom {
                missing_bytes: reserve + model.size_bytes - quota_bytes,
            }
        } else {
            reserve += model.size_bytes;
            plan.download.push(model.clone());
            LocalState::Pending
        };
        plan.states.push((model.file.clone(), state));
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIO: u64 = 1024 * 1024 * 1024;

    fn modele(file: &str, gio: u64) -> SharedModel {
        SharedModel {
            file: file.into(),
            size_bytes: gio * GIO,
            sha256: Some("ab".into()),
            modified_unix: 0,
        }
    }

    #[test]
    fn un_nom_venu_d_ailleurs_ne_sort_pas_de_la_bibliotheque() {
        assert!(is_safe_relative("Qwen3-4B.gguf"));
        assert!(is_safe_relative("qwen/Qwen3-32B-Q4_K_M.gguf"));
        assert!(!is_safe_relative("../secret.gguf"));
        assert!(!is_safe_relative("a/../../b.gguf"));
        assert!(!is_safe_relative("/etc/passwd.gguf"));
        assert!(!is_safe_relative("C:/Windows/x.gguf"));
        assert!(!is_safe_relative("modele.safetensors"));
        assert!(!is_safe_relative("a//b.gguf"));
    }

    #[test]
    fn ce_qui_manque_se_telecharge_dans_l_ordre_du_catalogue() {
        let catalog = Catalog {
            models: vec![modele("a.gguf", 4), modele("b.gguf", 8)],
        };
        let plan = plan_sync(
            &catalog,
            &BTreeMap::new(),
            &SyncLedger::default(),
            true,
            64 * GIO,
        );
        let fichiers: Vec<&str> = plan.download.iter().map(|m| m.file.as_str()).collect();
        assert_eq!(fichiers, vec!["a.gguf", "b.gguf"]);
        assert!(plan.remove.is_empty());
    }

    #[test]
    fn le_quota_arrete_ce_qui_ne_tient_pas_et_dit_combien_il_manque() {
        let catalog = Catalog {
            models: vec![modele("a.gguf", 10), modele("b.gguf", 10)],
        };
        let plan = plan_sync(
            &catalog,
            &BTreeMap::new(),
            &SyncLedger::default(),
            true,
            16 * GIO,
        );
        assert_eq!(plan.download.len(), 1);
        assert_eq!(
            plan.states[1].1,
            LocalState::NoRoom {
                missing_bytes: 4 * GIO
            }
        );
    }

    /// Un modèle que la personne avait déjà ne mange pas le quota accordé au
    /// cluster.
    #[test]
    fn un_modele_deja_la_ne_compte_pas_dans_le_quota() {
        let catalog = Catalog {
            models: vec![modele("perso.gguf", 20), modele("b.gguf", 10)],
        };
        let mut sizes = BTreeMap::new();
        sizes.insert("perso.gguf".to_string(), 20 * GIO);
        let plan = plan_sync(&catalog, &sizes, &SyncLedger::default(), true, 12 * GIO);
        assert_eq!(plan.states[0].1, LocalState::Ready);
        assert_eq!(plan.download.len(), 1);
        assert_eq!(plan.used_bytes, 0);
    }

    #[test]
    fn un_modele_plus_partage_est_supprime_s_il_a_ete_copie_par_ce_poste() {
        let catalog = Catalog {
            models: vec![modele("garde.gguf", 1)],
        };
        let mut ledger = SyncLedger::default();
        ledger.files.insert("garde.gguf".into());
        ledger.files.insert("retire.gguf".into());
        let plan = plan_sync(&catalog, &BTreeMap::new(), &ledger, true, 64 * GIO);
        assert_eq!(plan.remove, vec!["retire.gguf".to_string()]);
    }

    #[test]
    fn cesser_d_heberger_supprime_toutes_les_copies_du_cluster() {
        let catalog = Catalog {
            models: vec![modele("a.gguf", 1)],
        };
        let mut ledger = SyncLedger::default();
        ledger.files.insert("a.gguf".into());
        let plan = plan_sync(&catalog, &BTreeMap::new(), &ledger, false, 64 * GIO);
        assert_eq!(plan.remove, vec!["a.gguf".to_string()]);
        assert!(plan.download.is_empty());
        assert_eq!(plan.states[0].1, LocalState::NotHosted);
    }

    #[test]
    fn sans_empreinte_le_poste_attend_au_lieu_de_copier() {
        let mut m = modele("a.gguf", 1);
        m.sha256 = None;
        let catalog = Catalog { models: vec![m] };
        let plan = plan_sync(
            &catalog,
            &BTreeMap::new(),
            &SyncLedger::default(),
            true,
            64 * GIO,
        );
        assert!(plan.download.is_empty());
        assert_eq!(plan.states[0].1, LocalState::Preparing);
    }

    #[test]
    fn la_bibliotheque_se_lit_sur_deux_niveaux_sans_les_fragments() {
        let dir = std::env::temp_dir().join(format!("cluster-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("qwen").join("trop").join("profond")).unwrap();
        std::fs::write(dir.join("a.gguf"), b"12345").unwrap();
        std::fs::write(dir.join("qwen").join("b.gguf"), b"1").unwrap();
        std::fs::write(dir.join("qwen").join("b.gguf.part"), b"1").unwrap();
        std::fs::write(dir.join("qwen").join("trop").join("c.gguf"), b"1").unwrap();
        let lib = scan_library(&dir);
        let noms: Vec<&str> = lib.iter().map(|m| m.file.as_str()).collect();
        assert_eq!(noms, vec!["a.gguf", "qwen/b.gguf"]);
        assert_eq!(lib[0].size_bytes, 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn l_empreinte_d_un_fichier_est_celle_de_son_contenu() {
        let path = std::env::temp_dir().join(format!("cluster-sha-{}.gguf", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_file(&path);
    }
}
