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
        /// L'hôte du coordinateur, quand le code vient du serveur Locaryn
        /// auquel ce poste est connecté : l'agent le joint directement, sans
        /// attendre une balise que le réseau ne laisse pas toujours passer.
        #[serde(default)]
        coordinator: Option<String>,
    },
    /// Sur la machine du serveur Locaryn : crée le cluster s'il n'existe pas,
    /// se déclare coordinateur, et renvoie de quoi inscrire un poste client.
    Enroll,
    /// Quitte le cluster : oublie son identité, arrête de prêter, supprime
    /// les copies de modèles partagés faites par ce poste.
    Leave,
    /// Les préférences de partage de cette machine et ce qu'elles donnent.
    ShareGet,
    /// Enregistre les préférences de partage et les applique aussitôt.
    ShareSet {
        prefs: crate::sharing::SharePrefs,
    },
    /// Sur le coordinateur : la bibliothèque, ce qui en est partagé, et
    /// quelles machines en ont une copie.
    SharedModels,
    /// Sur le coordinateur : partager ou cesser de partager un modèle.
    ShareModel {
        file: String,
        enabled: bool,
    },
    /// Sur un poste client : le catalogue du coordinateur et l'état de
    /// chaque modèle sur ce poste.
    SyncStatus,
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
    Enrolled {
        cluster_name: String,
        pairing_code: String,
    },
    Left,
    Share {
        prefs: crate::sharing::SharePrefs,
        /// Les appareils que llama.cpp voit sur cette machine.
        devices: Vec<crate::sharing::RpcDevice>,
        worker_active: bool,
        rpc_port: Option<u16>,
        /// Ce qui empêche de prêter, quand la case est cochée mais que rien
        /// ne tourne.
        problem: Option<String>,
    },
    SharedModels {
        models: Vec<SharedModelView>,
        members: Vec<MemberView>,
    },
    Sync {
        coordinator: Option<String>,
        /// La dernière lecture du catalogue a-t-elle abouti ?
        reachable: bool,
        models: Vec<SyncEntry>,
        used_bytes: u64,
        quota_bytes: u64,
        last_error: Option<String>,
    },
    ShuttingDown,
    Error {
        message: String,
    },
}

/// Un modèle de la bibliothèque du coordinateur, vu depuis le panneau.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedModelView {
    pub file: String,
    pub size_bytes: u64,
    pub shared: bool,
    /// L'empreinte est calculée : les postes peuvent le copier.
    pub ready_to_copy: bool,
    /// Les machines qui en ont une copie complète.
    pub hosted_by: Vec<String>,
}

/// Une machine du cluster, vue depuis le coordinateur.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberView {
    pub name: String,
    pub address: String,
    pub sharing: bool,
    pub gpu: bool,
    pub ram: bool,
    pub storage: bool,
    pub lendable_gb: f32,
    pub last_seen_unix: u64,
    /// Vrai pour la machine qui répond — le coordinateur ajoute sa propre
    /// ligne, sans quoi les totaux du tableau de bord manqueraient sa part.
    #[serde(default)]
    pub is_self: bool,
    /// Ce qui suit vient tel quel de la dernière capacité annoncée par ce
    /// pair — le tableau de bord de performance du panneau. Zéro partout
    /// chez un pair jamais vu ou trop ancien pour l'annoncer.
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub gpu_name: Option<String>,
    #[serde(default)]
    pub free_vram_gb: f32,
    #[serde(default)]
    pub total_vram_gb: f32,
    #[serde(default)]
    pub free_ram_gb: f32,
    #[serde(default)]
    pub total_ram_gb: f32,
    #[serde(default)]
    pub cpu_usage_percent: f32,
}

/// L'état d'un modèle partagé sur ce poste.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncEntry {
    pub file: String,
    pub size_bytes: u64,
    #[serde(flatten)]
    pub state: crate::catalog::LocalState,
    /// Octets déjà sur le disque pendant une copie.
    pub copied_bytes: u64,
    /// La copie en cours, s'il y en a une.
    pub downloading: bool,
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
