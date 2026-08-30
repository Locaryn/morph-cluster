//! Le protocole de contrôle local entre l'agent (processus de fond persistant)
//! et ses deux clients : le serveur MCP (outils que le modèle et l'utilisateur
//! appellent) et le lanceur du moteur (`locaryn-cluster-launch`, invoqué par
//! le superviseur de Locaryn).
//!
//! Pourquoi un agent séparé plutôt que tout faire dans le serveur MCP
//! stdio : la découverte réseau (balise UDP périodique, écoute permanente du
//! canal de contrôle) doit tourner en continu, indépendamment de tout appel
//! d'outil. Un serveur MCP stdio ne vit que le temps d'une requête à l'autre
//! sans garantie de rester en mémoire ; l'agent, lui, est un processus de
//! fond que le premier appel démarre et que les suivants retrouvent.
//!
//! Le canal est du JSON délimité par des retours à la ligne sur une connexion
//! TCP loopback — le même choix que le reste de l'application fait pour tout
//! ce qui parle en local.

use crate::{Capability, Peer};
use serde::{Deserialize, Serialize};

/// Où l'agent note son port de contrôle et son PID, pour que le serveur MCP
/// et le lanceur le retrouvent sans avoir à deviner un port.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHandle {
    pub pid: u32,
    pub control_port: u16,
}

pub fn handle_file(state_dir: &std::path::Path) -> std::path::PathBuf {
    state_dir.join("agent.json")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum AgentRequest {
    /// L'état complet : identité (sans le secret), pairs connus, rôle.
    Status,
    /// Crée une nouvelle identité de cluster sur cette machine, en
    /// remplacement de la précédente le cas échéant.
    CreateCluster {
        name: String,
    },
    /// Rejoint un cluster existant à partir d'un code de pairage.
    JoinCluster {
        pairing_code: String,
    },
    /// Le code de pairage courant, pour le partager avec une autre machine.
    PairingCode,
    /// Les pairs actuellement connus, avec leur dernière capacité rafraîchie.
    Peers,
    /// Démarre `ggml-rpc-server` localement : cette machine se met à offrir
    /// son GPU au cluster.
    WorkerStart,
    WorkerStop,
    /// Calcule la répartition d'un modèle de cette taille sur les pairs
    /// actuellement joignables.
    Plan {
        model_size_gb: f32,
    },
    /// Mesure l'aller-retour de contrôle vers un pair, et met à jour sa fiche.
    Bench {
        peer_id: String,
    },
    /// Arrête proprement l'agent (retire la balise, ferme les sockets).
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ok", rename_all = "snake_case")]
pub enum AgentResponse {
    Status {
        cluster_name: Option<String>,
        cluster_id: Option<String>,
        self_capability: Capability,
        worker_active: bool,
        rpc_port: Option<u16>,
        peer_count: usize,
    },
    Created {
        pairing_code: String,
    },
    Joined {
        cluster_name: String,
    },
    PairingCode {
        pairing_code: String,
    },
    Peers {
        peers: Vec<Peer>,
    },
    WorkerStarted {
        port: u16,
    },
    WorkerStopped,
    Plan {
        plan: crate::plan::ClusterPlan,
    },
    Benched {
        peer_id: String,
        round_trip_ms: u32,
    },
    ShuttingDown,
    Error {
        message: String,
    },
}

/// Encode une requête en une ligne JSON terminée par `\n`.
pub fn encode_request(req: &AgentRequest) -> Vec<u8> {
    let mut line = serde_json::to_vec(req).unwrap_or_default();
    line.push(b'\n');
    line
}

pub fn encode_response(res: &AgentResponse) -> Vec<u8> {
    let mut line = serde_json::to_vec(res).unwrap_or_default();
    line.push(b'\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn une_requete_encodee_se_termine_par_un_saut_de_ligne() {
        let bytes = encode_request(&AgentRequest::Status);
        assert_eq!(*bytes.last().unwrap(), b'\n');
        let sans_saut = &bytes[..bytes.len() - 1];
        let relu: AgentRequest = serde_json::from_slice(sans_saut).unwrap();
        assert!(matches!(relu, AgentRequest::Status));
    }

    #[test]
    fn create_cluster_conserve_le_nom() {
        let req = AgentRequest::CreateCluster {
            name: "atelier".into(),
        };
        let bytes = encode_request(&req);
        let relu: AgentRequest = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        match relu {
            AgentRequest::CreateCluster { name } => assert_eq!(name, "atelier"),
            other => panic!("mauvaise variante : {other:?}"),
        }
    }

    #[test]
    fn une_erreur_se_distingue_d_un_succes_par_l_etiquette() {
        let res = AgentResponse::Error {
            message: "hors service".into(),
        };
        let json = serde_json::to_string(&res).unwrap();
        assert!(json.contains(r#""ok":"error""#));
    }
}
