//! Répartir un modèle sur plusieurs machines, ou non.
//!
//! Découper un modèle entre deux PC laisse passer un modèle qu'aucun ne porte
//! seul, mais chaque jeton traverse alors le réseau : c'est plus lent que
//! n'importe quelle machine seule. Tant que le modèle tient sur cette machine,
//! il y tourne donc **entier**, à pleine vitesse — même quand des pairs sont
//! disponibles. Le découpage n'entre en jeu que pour un modèle qui ne tient
//! pas, et seulement si la personne l'a autorisé : c'est un choix, pas un
//! effet de bord de l'appartenance au cluster.

use crate::plan::{self, ClusterPlan};
use crate::{Capability, Peer};
use serde::{Deserialize, Serialize};
use std::path::Path;

const FICHIER: &str = "routing.json";

/// Le choix de la personne sur cette machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RoutingConfig {
    /// Autoriser le découpage d'un modèle sur plusieurs machines. Faux par
    /// défaut : sans ce choix explicite, un modèle trop gros est refusé avec
    /// un message clair plutôt que d'être ralenti en silence.
    #[serde(default)]
    pub split_models: bool,
}

impl RoutingConfig {
    /// Sans fichier, ou avec un fichier illisible : le défaut, découpage
    /// interdit. Un choix perdu ne doit jamais réactiver un mode plus lent.
    pub fn load(dir: &Path) -> Self {
        std::fs::read_to_string(dir.join(FICHIER))
            .ok()
            .and_then(|texte| serde_json::from_str(&texte).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let json = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        std::fs::write(dir.join(FICHIER), json)
    }
}

/// Ce que le lancement d'un modèle va faire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum LaunchDecision {
    /// Le modèle tient sur cette machine : il y tourne entier, sans réseau.
    Local { model_size_gb: f32, usable_gb: f32 },
    /// Il ne tient pas ici, le découpage est autorisé et les pairs suffisent.
    Split { plan: ClusterPlan },
    /// Il ne peut pas être lancé, et pourquoi.
    Refuse { message: String },
}

impl LaunchDecision {
    /// Le nom court du mode, pour les journaux.
    pub fn label(&self) -> &'static str {
        match self {
            LaunchDecision::Local { .. } => "local",
            LaunchDecision::Split { .. } => "découpé",
            LaunchDecision::Refuse { .. } => "refusé",
        }
    }
}

impl Capability {
    /// La mémoire dans laquelle un modèle lancé sur cette machine tournera :
    /// la VRAM libre quand il y a une carte, la mémoire vive libre sinon —
    /// `llama-server` calcule alors sur le processeur, plus lentement mais
    /// sans réseau.
    pub fn run_capacity_gb(&self) -> f32 {
        if self.gpu_name.is_none() && self.total_vram_gb <= 0.0 {
            self.free_ram_gb
        } else {
            self.free_vram_gb
        }
    }
}

/// Décide comment lancer un modèle de cette taille.
///
/// `local_capacity_gb` est ce que la machine qui va piloter la conversation
/// peut porter seule ([`Capability::run_capacity_gb`]).
pub fn decide(
    model_size_gb: f32,
    local_capacity_gb: f32,
    peers: &[Peer],
    config: RoutingConfig,
    latence_max_ms: Option<u32>,
) -> LaunchDecision {
    let usable_gb = (local_capacity_gb * (1.0 - plan::MARGE_SECURITE)).max(0.0);
    if model_size_gb <= usable_gb {
        return LaunchDecision::Local {
            model_size_gb,
            usable_gb,
        };
    }

    if config.split_models {
        let plan = plan::compute(model_size_gb, local_capacity_gb, peers, latence_max_ms);
        if plan.sufficient {
            return LaunchDecision::Split { plan };
        }
        return LaunchDecision::Refuse {
            message: format!(
                "Ce modèle ({model_size_gb:.1} Go) ne tient pas, même découpé : {:.1} Go \
                 utilisables sur ce poste et les pairs retenus, il manque {:.1} Go. Ajoutez un \
                 pair (cluster_peers) ou faites-lui prêter sa carte (cluster_worker_start).",
                plan.usable_capacity_gb, plan.shortfall_gb
            ),
        };
    }

    // Découpage interdit : dire ce qui manque, et si un autre poste porterait
    // le modèle à lui seul — c'est la voie rapide, à préférer au découpage.
    let mut message = format!(
        "Ce modèle ({model_size_gb:.1} Go) ne tient pas sur cette machine ({usable_gb:.1} Go \
         utilisables), et le découpage sur plusieurs PC est désactivé."
    );
    if let Some(pair) = meilleur_pair_seul(model_size_gb, peers) {
        message.push_str(&format!(
            " « {} » a assez de mémoire ({:.1} Go) pour l'exécuter seul : lancez-le sur ce poste.",
            pair.capability.name,
            pair.capability.run_capacity_gb() * (1.0 - plan::MARGE_SECURITE)
        ));
    }
    message.push_str(
        " Pour le répartir quand même, activez « Découper un gros modèle sur plusieurs machines » \
         (plus lent : chaque jeton traverse le réseau).",
    );
    LaunchDecision::Refuse { message }
}

/// Le pair qui porterait ce modèle à lui seul, le plus spacieux d'abord.
fn meilleur_pair_seul(model_size_gb: f32, peers: &[Peer]) -> Option<&Peer> {
    peers
        .iter()
        .filter(|p| p.capability.run_capacity_gb() * (1.0 - plan::MARGE_SECURITE) >= model_size_gb)
        .max_by(|a, b| {
            a.capability
                .run_capacity_gb()
                .partial_cmp(&b.capability.run_capacity_gb())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(nom: &str, vram: f32) -> Peer {
        Peer {
            id: nom.to_string(),
            address: "192.168.1.20:41338".to_string(),
            rpc_port: Some(50052),
            capability: Capability {
                name: nom.to_string(),
                os: "linux".into(),
                arch: "x86_64".into(),
                gpu_name: Some("RTX 4050".into()),
                free_vram_gb: vram,
                has_llama_server: true,
                has_rpc_server: true,
                offered_memory_gb: 0.0,
                total_vram_gb: vram,
                total_ram_gb: 32.0,
                free_ram_gb: 16.0,
                cpu_usage_percent: 0.0,
                backend: None,
            },
            last_seen_unix: 0,
            round_trip_ms: Some(2),
            sharing: None,
        }
    }

    fn autorise() -> RoutingConfig {
        RoutingConfig { split_models: true }
    }

    #[test]
    fn le_decoupage_est_interdit_par_defaut() {
        assert!(!RoutingConfig::default().split_models);
    }

    /// Le cœur de la demande : un modèle qui tient ici tourne entier ici, même
    /// avec un pair disponible et le découpage autorisé. Le lancer en RPC
    /// serait plus lent sans rien apporter.
    #[test]
    fn un_modele_qui_tient_ici_ne_quitte_pas_la_machine() {
        let peers = vec![pair("autre", 24.0)];
        for config in [RoutingConfig::default(), autorise()] {
            let decision = decide(4.0, 8.0, &peers, config, None);
            assert!(
                matches!(decision, LaunchDecision::Local { .. }),
                "{decision:?}"
            );
        }
    }

    #[test]
    fn un_modele_trop_gros_est_refuse_si_le_decoupage_est_interdit() {
        let peers = vec![pair("autre", 8.0)];
        let LaunchDecision::Refuse { message } =
            decide(12.0, 8.0, &peers, RoutingConfig::default(), None)
        else {
            panic!("un modèle trop gros doit être refusé");
        };
        assert!(message.contains("désactivé"), "{message}");
        assert!(message.contains("Découper un gros modèle"), "{message}");
    }

    /// Le refus doit orienter : un pair qui porterait le modèle seul est la
    /// voie rapide, et le message le nomme.
    #[test]
    fn le_refus_nomme_le_pair_qui_porterait_le_modele_seul() {
        let peers = vec![pair("petit", 6.0), pair("grosse-station", 24.0)];
        let LaunchDecision::Refuse { message } =
            decide(12.0, 8.0, &peers, RoutingConfig::default(), None)
        else {
            panic!("refus attendu");
        };
        assert!(message.contains("grosse-station"), "{message}");
        assert!(!message.contains("« petit »"), "{message}");
    }

    #[test]
    fn un_modele_trop_gros_est_decoupe_si_c_est_autorise_et_suffisant() {
        let peers = vec![pair("autre", 24.0)];
        let decision = decide(16.0, 8.0, &peers, autorise(), None);
        let LaunchDecision::Split { plan } = decision else {
            panic!("découpage attendu");
        };
        assert_eq!(plan.members.len(), 1);
    }

    #[test]
    fn un_decoupage_autorise_mais_insuffisant_est_refuse_avec_le_manque() {
        let peers = vec![pair("autre", 8.0)];
        let LaunchDecision::Refuse { message } = decide(30.0, 8.0, &peers, autorise(), None) else {
            panic!("refus attendu");
        };
        assert!(message.contains("il manque"), "{message}");
    }

    /// Sans carte graphique, `llama-server` tourne sur le processeur : la
    /// mémoire vive fait la capacité, pas une VRAM nulle.
    #[test]
    fn sans_carte_la_memoire_vive_fait_la_capacite() {
        let mut sans_carte = pair("portable", 0.0).capability;
        sans_carte.gpu_name = None;
        sans_carte.total_vram_gb = 0.0;
        sans_carte.free_ram_gb = 16.0;
        assert_eq!(sans_carte.run_capacity_gb(), 16.0);

        let mut avec_carte = sans_carte.clone();
        avec_carte.gpu_name = Some("RTX 4050".into());
        avec_carte.total_vram_gb = 6.0;
        avec_carte.free_vram_gb = 5.0;
        assert_eq!(avec_carte.run_capacity_gb(), 5.0);
    }

    #[test]
    fn le_choix_survit_a_un_redemarrage() {
        let dossier = std::env::temp_dir().join(format!(
            "locaryn_mode_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(
            !RoutingConfig::load(&dossier).split_models,
            "pas de fichier : interdit"
        );

        autorise().save(&dossier).unwrap();
        assert!(RoutingConfig::load(&dossier).split_models);

        std::fs::write(dossier.join(FICHIER), b"pas du json").unwrap();
        assert!(
            !RoutingConfig::load(&dossier).split_models,
            "un fichier illisible ne doit pas réactiver le mode lent"
        );
        let _ = std::fs::remove_dir_all(&dossier);
    }
}
