//! Calcule comment répartir un modèle entre les machines du cluster.
//!
//! `llama-server --rpc` ne demande qu'une liste `hôte:port` : c'est lui qui
//! décide, en interne, combien de couches confier à chaque entrée de la
//! liste, à partir de la mémoire libre que chaque `ggml-rpc-server` annonce
//! au moment de la connexion. Ce module ne refait donc pas ce calcul — il
//! décide en amont **quels pairs inclure**, dans quel ordre, et si leur
//! mémoire cumulée a une chance de suffire, pour échouer avant de lancer un
//! chargement de plusieurs dizaines de gigaoctets plutôt qu'au milieu.

use crate::Peer;
use serde::{Deserialize, Serialize};

/// Marge de sécurité retirée de la VRAM libre annoncée par chaque pair.
///
/// La mémoire libre mesurée à l'instant de l'annonce n'est pas celle qui
/// restera au moment du chargement — le système d'exploitation, l'affichage,
/// une autre application en prennent un peu. Réserver 10 % évite un échec de
/// chargement à 95 % rempli.
const MARGE_SECURITE: f32 = 0.10;

/// Le plan calculé pour un modèle donné.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterPlan {
    /// Taille du modèle, telle que fournie à [`compute`].
    pub model_size_gb: f32,
    /// Capacité totale utilisable (après marge), tous pairs retenus compris.
    pub usable_capacity_gb: f32,
    /// Pairs retenus, dans l'ordre où ils seront passés à `--rpc` — les plus
    /// spacieux d'abord, pour que le premier échec de connexion (le plus
    /// probable étant le plus proche) coûte le moins de couches à
    /// redistribuer.
    pub members: Vec<PlanMember>,
    /// Pairs exclus, et pourquoi — un pair sans `ggml-rpc-server`, ou dont la
    /// liaison est jugée trop lente (voir `round_trip_ms`).
    pub excluded: Vec<ExcludedPeer>,
    /// `false` si la capacité retenue ne suffit pas : le plan est renvoyé
    /// quand même, pour que l'appelant voie le manque exact plutôt qu'un
    /// simple refus.
    pub sufficient: bool,
    /// Ce qui manque, en gigaoctets, quand `sufficient` est faux.
    pub shortfall_gb: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanMember {
    pub peer_id: String,
    pub address: String,
    pub rpc_port: u16,
    pub usable_vram_gb: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExcludedPeer {
    pub peer_id: String,
    pub reason: String,
}

/// Au-delà de cette latence de contrôle, un pair est exclu par défaut : le
/// canal RPC de llama.cpp échange des tenseurs à chaque étage du calcul, et
/// un aller-retour de contrôle déjà lent laisse peu d'espoir pour la charge
/// utile, bien plus lourde.
const LATENCE_MAX_MS_PAR_DEFAUT: u32 = 150;

/// Calcule le plan. `local_free_vram_gb` est la VRAM de la machine qui va
/// lancer `llama-server` elle-même — elle porte sa part du modèle sans passer
/// par le réseau, et compte donc dans la capacité totale.
pub fn compute(
    model_size_gb: f32,
    local_free_vram_gb: f32,
    peers: &[Peer],
    latence_max_ms: Option<u32>,
) -> ClusterPlan {
    let latence_max = latence_max_ms.unwrap_or(LATENCE_MAX_MS_PAR_DEFAUT);
    let mut members = Vec::new();
    let mut excluded = Vec::new();

    let mut candidats: Vec<&Peer> = peers.iter().collect();
    candidats.sort_by(|a, b| {
        b.capability
            .lendable_gb()
            .partial_cmp(&a.capability.lendable_gb())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    for peer in candidats {
        if !peer.capability.can_serve() {
            excluded.push(ExcludedPeer {
                peer_id: peer.id.clone(),
                reason: "n'offre pas ggml-rpc-server, ou VRAM libre négligeable".to_string(),
            });
            continue;
        }
        let Some(rpc_port) = peer.rpc_port else {
            excluded.push(ExcludedPeer {
                peer_id: peer.id.clone(),
                reason: "connu mais n'a pas démarré son serveur RPC (cluster_worker_start)"
                    .to_string(),
            });
            continue;
        };
        if let Some(rtt) = peer.round_trip_ms {
            if rtt > latence_max {
                excluded.push(ExcludedPeer {
                    peer_id: peer.id.clone(),
                    reason: format!(
                        "liaison trop lente ({rtt} ms, seuil {latence_max} ms) — le RPC de \
                         llama.cpp est bavard et ralentirait probablement plus qu'il n'aide"
                    ),
                });
                continue;
            }
        }
        let usable = (peer.capability.lendable_gb() * (1.0 - MARGE_SECURITE)).max(0.0);
        members.push(PlanMember {
            peer_id: peer.id.clone(),
            address: peer.address.clone(),
            rpc_port,
            usable_vram_gb: usable,
        });
    }

    let local_usable = (local_free_vram_gb * (1.0 - MARGE_SECURITE)).max(0.0);
    let usable_capacity_gb = local_usable + members.iter().map(|m| m.usable_vram_gb).sum::<f32>();

    let sufficient = usable_capacity_gb >= model_size_gb;
    let shortfall_gb = if sufficient {
        0.0
    } else {
        model_size_gb - usable_capacity_gb
    };

    ClusterPlan {
        model_size_gb,
        usable_capacity_gb,
        members,
        excluded,
        sufficient,
        shortfall_gb,
    }
}

impl ClusterPlan {
    /// La valeur de l'argument `--rpc` de `llama-server` : la liste des
    /// pairs retenus, dans l'ordre du plan.
    pub fn rpc_argument(&self) -> String {
        self.members
            .iter()
            .map(|m| format!("{}:{}", peer_host(&m.address), m.rpc_port))
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// L'adresse d'un pair peut porter son propre port de contrôle
/// (`192.168.1.20:41338`) : seul l'hôte nous intéresse ici, le port RPC est
/// annoncé séparément.
fn peer_host(address: &str) -> &str {
    address.rsplit_once(':').map(|(h, _)| h).unwrap_or(address)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Capability;

    /// `has_rpc_server` (le binaire est présent) et `rpc_port` (le serveur
    /// est démarré) sont deux choses distinctes — un pair connu peut avoir
    /// l'un sans l'autre. Ce constructeur les traite indépendamment plutôt
    /// que de déduire l'un de l'autre, pour que les tests puissent exercer
    /// les deux motifs d'exclusion séparément.
    fn pair(nom: &str, vram: f32, rpc_port: Option<u16>, rtt: Option<u32>) -> Peer {
        pair_avec_binaire(nom, vram, true, rpc_port, rtt)
    }

    fn pair_avec_binaire(
        nom: &str,
        vram: f32,
        has_rpc_server: bool,
        rpc_port: Option<u16>,
        rtt: Option<u32>,
    ) -> Peer {
        Peer {
            id: nom.to_string(),
            address: format!("192.168.1.{}:41338", nom.len() + 10),
            rpc_port,
            capability: Capability {
                name: nom.to_string(),
                os: "linux".into(),
                arch: "x86_64".into(),
                gpu_name: Some("RTX 4050".into()),
                free_vram_gb: vram,
                has_llama_server: true,
                has_rpc_server,
                offered_memory_gb: 0.0,
            },
            last_seen_unix: 0,
            round_trip_ms: rtt,
            sharing: None,
        }
    }

    /// L'exemple même de l'utilisateur : un modèle de 16 Go, deux cartes de
    /// 8 Go — sans marge de sécurité ça collerait pile, avec elle ça doit
    /// légèrement manquer, ce qui est le comportement honnête à montrer.
    #[test]
    fn seize_go_sur_deux_cartes_de_huit_montre_la_marge() {
        let peers = vec![pair("b", 8.0, Some(50052), Some(2))];
        let plan = compute(16.0, 8.0, &peers, None);
        assert_eq!(plan.members.len(), 1);
        assert!(
            !plan.sufficient,
            "8+8 sans marge ne suffit pas avec la marge de 10 %"
        );
        assert!(plan.shortfall_gb > 0.0 && plan.shortfall_gb < 2.0);
    }

    #[test]
    fn une_capacite_large_est_jugee_suffisante() {
        let peers = vec![pair("b", 24.0, Some(50052), Some(2))];
        let plan = compute(16.0, 8.0, &peers, None);
        assert!(plan.sufficient);
        assert_eq!(plan.shortfall_gb, 0.0);
    }

    #[test]
    fn un_pair_sans_serveur_rpc_est_exclu_avec_la_raison() {
        // Le binaire est là, mais le serveur n'a pas été démarré : c'est le
        // motif d'exclusion « cluster_worker_start », distinct de « ne
        // possède pas le binaire ».
        let peers = vec![pair_avec_binaire("sans-rpc", 24.0, true, None, Some(2))];
        let plan = compute(16.0, 8.0, &peers, None);
        assert!(plan.members.is_empty());
        assert_eq!(plan.excluded.len(), 1);
        assert!(plan.excluded[0].reason.contains("cluster_worker_start"));
    }

    #[test]
    fn un_pair_sans_le_binaire_rpc_est_exclu_pour_cette_raison() {
        let peers = vec![pair_avec_binaire(
            "sans-binaire",
            24.0,
            false,
            None,
            Some(2),
        )];
        let plan = compute(16.0, 8.0, &peers, None);
        assert!(plan.members.is_empty());
        assert!(plan.excluded[0].reason.contains("ggml-rpc-server"));
    }

    #[test]
    fn un_pair_trop_lent_est_exclu() {
        let peers = vec![pair("lointain", 24.0, Some(50052), Some(400))];
        let plan = compute(16.0, 8.0, &peers, None);
        assert!(plan.members.is_empty());
        assert!(plan.excluded[0].reason.contains("lente"));
    }

    #[test]
    fn les_membres_sont_ordonnes_du_plus_spacieux_au_moins_spacieux() {
        let peers = vec![
            pair("petit", 4.0, Some(1), Some(2)),
            pair("grand", 20.0, Some(2), Some(2)),
            pair("moyen", 10.0, Some(3), Some(2)),
        ];
        let plan = compute(1.0, 0.0, &peers, None);
        let noms: Vec<&str> = plan.members.iter().map(|m| m.peer_id.as_str()).collect();
        assert_eq!(noms, vec!["grand", "moyen", "petit"]);
    }

    #[test]
    fn l_argument_rpc_est_une_liste_hote_port_separee_par_des_virgules() {
        let peers = vec![
            pair("a", 20.0, Some(50052), Some(2)),
            pair("b", 20.0, Some(50053), Some(2)),
        ];
        let plan = compute(1.0, 0.0, &peers, None);
        let arg = plan.rpc_argument();
        assert!(arg.contains(":50052"));
        assert!(arg.contains(":50053"));
        assert_eq!(arg.matches(',').count(), 1);
    }

    #[test]
    fn sans_aucun_pair_le_manque_est_calcule_sur_la_seule_machine_locale() {
        let plan = compute(16.0, 6.0, &[], None);
        assert!(!plan.sufficient);
        // 6 * 0.9 = 5.4 utilisables ; il manque 10.6 Go.
        assert!((plan.shortfall_gb - 10.6).abs() < 0.05);
    }
}
