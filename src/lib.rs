//! Extension Cluster — mettre en commun la VRAM de plusieurs machines pour
//! faire tourner un modèle GGUF trop gros pour une seule carte.
//!
//! Le calcul lui-même est fait par **llama.cpp** : `ggml-rpc-server` sur les
//! machines qui prêtent leur GPU, `llama-server --rpc host:port,…` sur celle
//! qui répond aux conversations. Ce moteur existe dans llama.cpp depuis
//! longtemps ; ce que cette extension ajoute, c'est tout ce qui manquait
//! autour : trouver les autres machines, s'assurer qu'elles font partie du
//! même cluster avant de leur envoyer des couches de poids, calculer combien
//! chacune peut en porter, et démarrer les bons processus au bon moment.
//!
//! **Ce que ce module ne fait pas** : parler à un tunnel, une passerelle
//! VPN ou un accès distant en particulier. Un pair est une adresse
//! `hôte:port` ; comment cette adresse est devenue joignable — réseau local,
//! VPN, tunnel — ne regarde pas ce code. C'est ce qui le rend compatible avec
//! n'importe quelle extension de connectivité, présente ou future, sans
//! qu'une ligne d'ici ne la nomme.
//!
//! **Sécurité, sans détour** : le pairage (identité de cluster + défi HMAC)
//! empêche une machine qui ne connaît pas le secret d'apparaître comme pair.
//! Mais `ggml-rpc-server` lui-même — le programme de llama.cpp qui reçoit les
//! couches — ne sait pas s'authentifier : n'importe qui capable d'atteindre
//! son port peut lui parler. Cette extension limite la fenêtre (le serveur
//! RPC ne tourne que pendant une session active, sur un port choisi au hasard
//! et communiqué seulement après authentification) mais **n'invente pas une
//! sécurité que llama.cpp n'a pas** : à réserver à un réseau de confiance.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub mod agent_client;
pub mod agent_protocol;
pub mod catalog;
pub mod discovery;
pub mod hmac_auth;
pub mod identity;
pub mod launch;
pub mod peer_channel;
pub mod plan;
pub mod sharing;
pub mod transfer;

/// Version du protocole de découverte et de pairage. Change si le format des
/// messages change — une machine qui ne la reconnaît pas ignore le message
/// plutôt que de mal l'interpréter.
pub const PROTOCOL_VERSION: u8 = 1;

/// Port UDP de la balise de découverte locale (diffusion réseau).
pub const DISCOVERY_UDP_PORT: u16 = 41337;

/// Port TCP du canal de contrôle d'un pair, par défaut. Chaque agent peut en
/// choisir un autre ; le port réel est celui annoncé dans la balise.
pub const DEFAULT_CONTROL_PORT: u16 = 41338;

/// Port par défaut du moteur exposé à Locaryn (voir `engine.port` du
/// manifeste — dupliqué ici pour que le lanceur n'ait pas à reparser le JSON).
pub const DEFAULT_ENGINE_PORT: u16 = 19190;

/// Le dossier privé de l'extension, donné par l'hôte.
pub fn extension_data_dir() -> PathBuf {
    std::env::var_os("LOCARYN_EXTENSION_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("locaryn-cluster"))
}

/// Où l'agent range son état : identité du cluster, pairs connus, PID.
pub fn state_dir() -> PathBuf {
    extension_data_dir().join("state")
}

/// Où loger les binaires llama.cpp que l'extension gère elle-même, quand
/// aucune installation existante n'a été trouvée.
pub fn vendor_dir() -> PathBuf {
    extension_data_dir().join("llama")
}

/// La bibliothèque de poids de l'utilisateur, donnée par l'hôte.
pub fn models_dir() -> Option<PathBuf> {
    std::env::var_os("LOCARYN_MODELS_DIR").map(PathBuf::from)
}

/// Une capacité annoncée par une machine — la sienne, ou celle d'un pair
/// appris par la découverte. C'est sur ce nombre que la répartition des
/// couches se décide : mieux vaut une valeur honnête et prudente qu'un
/// optimisme qui fait échouer le chargement à mi-parcours.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capability {
    /// Nom affiché de la machine (nom d'hôte, par défaut).
    pub name: String,
    pub os: String,
    pub arch: String,
    /// Nom du GPU, si connu (`nvidia-smi --query-gpu=name`).
    pub gpu_name: Option<String>,
    /// VRAM libre en gibioctets, mesurée au moment de l'annonce — pas la
    /// VRAM totale : ce qu'un autre modèle occupe déjà ne peut pas servir.
    pub free_vram_gb: f32,
    /// `llama-server` est présent et joignable.
    pub has_llama_server: bool,
    /// `ggml-rpc-server` est présent et joignable.
    pub has_rpc_server: bool,
    /// La mémoire que son serveur RPC offre réellement, en gibioctets : la
    /// VRAM libre des cartes prêtées, plus la mémoire vive si elle l'est
    /// aussi. Zéro chez une machine qui n'a pas enregistré de préférences de
    /// partage — la VRAM libre fait alors foi, comme avant.
    #[serde(default)]
    pub offered_memory_gb: f32,
    /// VRAM totale de la carte retenue, en gibioctets. Zéro : inconnue (pas
    /// de `nvidia-smi`, aucun appareil GPU annoncé par llama.cpp) — le
    /// tableau de bord affiche alors la VRAM libre seule plutôt qu'une barre
    /// dont le total serait inventé.
    #[serde(default)]
    pub total_vram_gb: f32,
    /// Mémoire vive totale de la machine, en gibioctets.
    #[serde(default)]
    pub total_ram_gb: f32,
    /// Mémoire vive libre au moment de l'annonce, en gibioctets.
    #[serde(default)]
    pub free_ram_gb: f32,
    /// Charge CPU instantanée (moyenne tous cœurs), de 0 à 100. Un ancien
    /// pair qui ne l'annonce pas encore reste à zéro — pas une charge nulle
    /// réelle, une valeur absente.
    #[serde(default)]
    pub cpu_usage_percent: f32,
}

impl Capability {
    /// Ce que le calcul de répartition peut confier à cette machine, avant
    /// marge de sécurité.
    pub fn lendable_gb(&self) -> f32 {
        if self.offered_memory_gb > 0.0 {
            self.offered_memory_gb
        } else {
            self.free_vram_gb
        }
    }

    /// Cette machine peut-elle jouer le rôle de travailleur (prêter son
    /// GPU) ? Sans `ggml-rpc-server`, elle ne peut qu'observer le cluster.
    pub fn can_serve(&self) -> bool {
        self.has_rpc_server && self.lendable_gb() > 0.1
    }
}

/// Le dossier de la bibliothèque de poids, ou une erreur qui dit pourquoi il
/// manque — c'est l'hôte qui le donne, et un agent lancé à la main ne l'a pas.
pub fn require_models_dir() -> Result<PathBuf, String> {
    models_dir().ok_or_else(|| {
        "bibliothèque de poids inconnue : LOCARYN_MODELS_DIR n'est pas défini (l'agent doit \
         être lancé par Locaryn)"
            .to_string()
    })
}

/// Un pair tel que l'agent le connaît : sa capacité déclarée, où le joindre,
/// et depuis quand on ne l'a plus vu.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    /// Identifiant stable du pair (dérivé de son identité de cluster + un
    /// nonce local) — pas son adresse, qui peut changer (DHCP).
    pub id: String,
    pub address: String,
    /// Port sur lequel `ggml-rpc-server` répond **quand il tourne**. `None`
    /// s'il n'est pas démarré : le pair est connu mais n'offre rien pour
    /// l'instant.
    pub rpc_port: Option<u16>,
    pub capability: Capability,
    /// Secondes UNIX de la dernière fois où ce pair a répondu.
    pub last_seen_unix: u64,
    /// Aller-retour de contrôle mesuré au dernier échange, en millisecondes.
    /// Sert à avertir : le RPC de llama.cpp est bavard, et un lien lent
    /// (Wi-Fi faible, tunnel à forte latence) peut ralentir plus qu'il
    /// n'aide.
    pub round_trip_ms: Option<u32>,
    /// Ce que ce pair prête et héberge, tel qu'il l'a annoncé.
    #[serde(default)]
    pub sharing: Option<sharing::SharingAnnounce>,
}

/// Empreinte courte d'un identifiant de cluster, montrée dans la balise de
/// découverte.
///
/// La balise ne porte jamais le secret ni l'identifiant en clair : seule une
/// machine qui connaît déjà le secret peut vérifier que cette empreinte lui
/// correspond. Un curieux qui écoute le réseau local apprend qu'un cluster
/// existe, pas lequel.
pub fn beacon_fingerprint(cluster_id: &str, secret: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cluster_id.as_bytes());
    hasher.update(secret);
    hasher.update(b"locaryn-cluster-beacon-v1");
    let digest = hasher.finalize();
    hex_encode(&digest[..8])
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l_empreinte_est_stable_et_ne_revele_rien_en_clair() {
        let a = beacon_fingerprint("cluster-un", b"secret-un");
        let b = beacon_fingerprint("cluster-un", b"secret-un");
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(!a.contains("cluster-un"));
    }

    /// Deux clusters différents — même par le seul secret — ne partagent pas
    /// leur empreinte : sinon une machine du mauvais cluster répondrait à la
    /// balise.
    #[test]
    fn deux_secrets_donnent_deux_empreintes() {
        let a = beacon_fingerprint("cluster-un", b"secret-un");
        let b = beacon_fingerprint("cluster-un", b"secret-deux");
        assert_ne!(a, b);
    }

    #[test]
    fn une_machine_sans_rpc_server_ne_peut_pas_servir() {
        let cap = Capability {
            name: "essai".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            gpu_name: Some("RTX 4050".into()),
            free_vram_gb: 6.0,
            has_llama_server: true,
            has_rpc_server: false,
            offered_memory_gb: 0.0,
            total_vram_gb: 8.0,
            total_ram_gb: 32.0,
            free_ram_gb: 16.0,
            cpu_usage_percent: 0.0,
        };
        assert!(!cap.can_serve());
    }

    /// La mémoire vive prêtée compte : une machine sans carte mais qui prête
    /// 12 Go de RAM porte 12 Go de couches.
    #[test]
    fn la_memoire_offerte_prime_sur_la_vram_libre() {
        let cap = Capability {
            name: "essai".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            gpu_name: None,
            free_vram_gb: 0.0,
            has_llama_server: true,
            has_rpc_server: true,
            offered_memory_gb: 12.0,
            total_vram_gb: 0.0,
            total_ram_gb: 32.0,
            free_ram_gb: 16.0,
            cpu_usage_percent: 0.0,
        };
        assert!(cap.can_serve());
        assert_eq!(cap.lendable_gb(), 12.0);
    }
}
