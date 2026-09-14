//! Ce qu'une machine accepte de prêter au cluster : sa carte graphique, sa
//! mémoire vive, et de la place sur son disque pour héberger une copie des
//! modèles partagés.
//!
//! Ce choix appartient à la personne assise devant la machine — c'est la case
//! « Allouer cette machine au partage de ressources » de ses réglages de
//! compte. Tant qu'elle n'est pas cochée, un coordinateur authentifié ne peut
//! pas démarrer de serveur RPC ici : appartenir au cluster ne vaut pas
//! consentement à prêter sa machine.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Les préférences de partage de cette machine, telles que le panneau de
/// compte les enregistre.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SharePrefs {
    /// La case principale. Faux : rien n'est prêté, quels que soient les
    /// détails ci-dessous.
    #[serde(default)]
    pub enabled: bool,
    /// Prêter la carte graphique (VRAM).
    #[serde(default = "vrai")]
    pub gpu: bool,
    /// Prêter la mémoire vive, pour les couches qu'aucune carte n'accueille.
    #[serde(default)]
    pub ram: bool,
    /// Héberger une copie des modèles partagés sur ce disque.
    #[serde(default = "vrai")]
    pub storage: bool,
    /// Place maximale accordée aux copies, en gigaoctets.
    #[serde(default = "quota_par_defaut")]
    pub storage_gb: f32,
    /// L'hôte du serveur Locaryn qui a inscrit cette machine. Sa présence dit
    /// que cette machine est un poste client ; son absence, qu'elle ne suit
    /// aucun coordinateur.
    #[serde(default)]
    pub coordinator_host: Option<String>,
    /// Vrai sur la machine qui tient le catalogue : celle où tourne le serveur
    /// Locaryn auquel les postes clients se connectent.
    #[serde(default)]
    pub coordinator: bool,
}

fn vrai() -> bool {
    true
}

fn quota_par_defaut() -> f32 {
    64.0
}

impl Default for SharePrefs {
    fn default() -> Self {
        Self {
            enabled: false,
            gpu: true,
            ram: false,
            storage: true,
            storage_gb: quota_par_defaut(),
            coordinator_host: None,
            coordinator: false,
        }
    }
}

impl SharePrefs {
    /// `None` quand aucun choix n'a jamais été enregistré : c'est le cas d'un
    /// cluster monté à la main avec les outils (`cluster_worker_start`), qui
    /// garde son comportement d'avant les préférences.
    pub fn load(dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(dir.join("share.json")).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let json = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        std::fs::write(dir.join("share.json"), json)
    }

    /// Faut-il un serveur RPC sur cette machine ?
    pub fn lends_compute(&self) -> bool {
        self.enabled && (self.gpu || self.ram)
    }

    /// Faut-il héberger des copies des modèles partagés ?
    pub fn hosts_models(&self) -> bool {
        self.enabled && self.storage && self.storage_gb > 0.0
    }

    /// Le quota en octets.
    pub fn quota_bytes(&self) -> u64 {
        (self.storage_gb.max(0.0) as f64 * 1024.0 * 1024.0 * 1024.0) as u64
    }

    /// Un coordinateur authentifié peut-il démarrer un serveur RPC ici ?
    ///
    /// Sans préférences enregistrées, oui : c'est le cluster monté à la main,
    /// dont chaque machine a elle-même appelé `cluster_worker_start`.
    pub fn allows_remote_start(prefs: Option<&SharePrefs>) -> bool {
        prefs.is_none_or(SharePrefs::lends_compute)
    }
}

/// Ce que cette machine dit de son partage aux autres membres.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SharingAnnounce {
    pub enabled: bool,
    pub gpu: bool,
    pub ram: bool,
    pub storage: bool,
    /// Vrai sur la machine qui tient le catalogue.
    pub coordinator: bool,
    /// Les modèles partagés dont une copie complète et vérifiée est ici.
    pub models_ready: Vec<String>,
}

/// Un appareil de calcul tel que `ggml-rpc-server` le nomme.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcDevice {
    /// `CUDA0`, `Vulkan0`, `Metal`, `CPU`…
    pub name: String,
    pub description: String,
    pub total_mib: u64,
    pub free_mib: u64,
}

impl RpcDevice {
    pub fn is_cpu(&self) -> bool {
        self.name.eq_ignore_ascii_case("cpu") || self.name.to_ascii_uppercase().starts_with("CPU")
    }
}

/// Lit la liste d'appareils que `ggml-rpc-server` imprime quand on lui nomme
/// un appareil inconnu :
///
/// ```text
///   Vulkan0: NVIDIA GeForce RTX 4050 Laptop GPU (6141 MiB, 5230 MiB free)
///   CPU: 13th Gen Intel(R) Core(TM) i7-13620H (16012 MiB, 7342 MiB free)
/// ```
///
/// C'est la seule source qui connaît toutes les cartes — NVIDIA, AMD, Intel,
/// Apple — sous le nom exact que `-d` attend.
pub fn parse_device_list(output: &str) -> Vec<RpcDevice> {
    output.lines().filter_map(parse_device_line).collect()
}

fn parse_device_line(line: &str) -> Option<RpcDevice> {
    let line = line.trim();
    let (name, rest) = line.split_once(": ")?;
    if name.is_empty() || name.contains(' ') {
        return None;
    }
    let open = rest.rfind('(')?;
    let description = rest[..open].trim().to_string();
    let inside = rest[open + 1..].trim_end_matches(')');
    let mut nombres = inside
        .split(',')
        .map(|part| part.split_whitespace().next().unwrap_or(""))
        .map(|n| n.parse::<u64>().ok());
    let total_mib = nombres.next()??;
    let free_mib = nombres.next()??;
    Some(RpcDevice {
        name: name.to_string(),
        description,
        total_mib,
        free_mib,
    })
}

/// Les appareils à passer à `-d`, d'après ce que la machine accepte de prêter.
///
/// `Err` quand le choix ne laisse rien à prêter — la carte graphique seule
/// sur une machine qui n'en a pas : sans `-d`, `ggml-rpc-server` se rabattrait
/// en silence sur le processeur, et prêterait la mémoire vive que la personne
/// a justement refusé de prêter.
pub fn devices_to_lend(prefs: &SharePrefs, devices: &[RpcDevice]) -> Result<Vec<String>, String> {
    let noms: Vec<String> = devices
        .iter()
        .filter(|d| if d.is_cpu() { prefs.ram } else { prefs.gpu })
        .map(|d| d.name.clone())
        .collect();
    if noms.is_empty() {
        return Err(if prefs.gpu && !prefs.ram {
            "aucune carte graphique détectée par llama.cpp : cochez la mémoire vive pour prêter \
             cette machine quand même"
                .to_string()
        } else {
            "rien à prêter : cochez la carte graphique ou la mémoire vive".to_string()
        });
    }
    Ok(noms)
}

/// La mémoire que ces appareils offrent, en gigaoctets — ce que le calcul de
/// répartition compte pour cette machine.
pub fn offered_memory_gb(devices: &[RpcDevice], lent: &[String]) -> f32 {
    devices
        .iter()
        .filter(|d| lent.iter().any(|n| n == &d.name))
        .map(|d| d.free_mib as f32 / 1024.0)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTE: &str = "error: unknown device: ?\navailable devices:\n  \
        Vulkan0: NVIDIA GeForce RTX 4050 Laptop GPU (6141 MiB, 5230 MiB free)\n  \
        CPU: 13th Gen Intel(R) Core(TM) i7-13620H (16012 MiB, 7342 MiB free)\n";

    #[test]
    fn la_liste_d_appareils_se_lit_telle_que_llama_cpp_l_imprime() {
        let devices = parse_device_list(LISTE);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].name, "Vulkan0");
        assert_eq!(devices[0].free_mib, 5230);
        assert!(devices[1].is_cpu());
        assert_eq!(devices[1].total_mib, 16012);
    }

    #[test]
    fn la_carte_seule_ne_prete_pas_la_memoire_vive() {
        let devices = parse_device_list(LISTE);
        let prefs = SharePrefs {
            enabled: true,
            ..SharePrefs::default()
        };
        assert_eq!(devices_to_lend(&prefs, &devices).unwrap(), vec!["Vulkan0"]);
    }

    #[test]
    fn carte_et_memoire_vive_pretent_les_deux() {
        let devices = parse_device_list(LISTE);
        let prefs = SharePrefs {
            enabled: true,
            ram: true,
            ..SharePrefs::default()
        };
        let lent = devices_to_lend(&prefs, &devices).unwrap();
        assert_eq!(lent, vec!["Vulkan0", "CPU"]);
        let offert = offered_memory_gb(&devices, &lent);
        assert!((offert - (5230.0 + 7342.0) / 1024.0).abs() < 0.01);
    }

    /// Sans carte, « carte seule » doit refuser plutôt que laisser llama.cpp
    /// se rabattre sur le processeur.
    #[test]
    fn sans_carte_la_carte_seule_est_refusee() {
        let devices = parse_device_list("  CPU: processeur (8000 MiB, 4000 MiB free)\n");
        let prefs = SharePrefs {
            enabled: true,
            ..SharePrefs::default()
        };
        let erreur = devices_to_lend(&prefs, &devices).unwrap_err();
        assert!(erreur.contains("mémoire vive"));
    }

    #[test]
    fn une_ligne_etrangere_n_est_pas_un_appareil() {
        assert!(parse_device_list("Invalid parameters\nerror: unknown device: ?").is_empty());
    }

    #[test]
    fn sans_preferences_le_cluster_manuel_garde_son_comportement() {
        assert!(SharePrefs::allows_remote_start(None));
        let refuse = SharePrefs::default();
        assert!(!SharePrefs::allows_remote_start(Some(&refuse)));
        let accepte = SharePrefs {
            enabled: true,
            ..SharePrefs::default()
        };
        assert!(SharePrefs::allows_remote_start(Some(&accepte)));
    }

    #[test]
    fn une_case_decochee_ne_prete_rien_meme_avec_les_details_coches() {
        let prefs = SharePrefs {
            enabled: false,
            gpu: true,
            ram: true,
            storage: true,
            ..SharePrefs::default()
        };
        assert!(!prefs.lends_compute());
        assert!(!prefs.hosts_models());
    }

    #[test]
    fn des_preferences_anciennes_se_relisent_avec_leurs_valeurs_par_defaut() {
        let prefs: SharePrefs = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert!(prefs.gpu && prefs.storage && !prefs.ram);
        assert_eq!(prefs.storage_gb, 64.0);
    }
}
