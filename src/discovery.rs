//! Formats des messages de découverte et de contrôle.
//!
//! La découverte a deux étages : une **balise** UDP, diffusée périodiquement,
//! qui ne révèle qu'une empreinte du cluster ([`crate::beacon_fingerprint`]) ;
//! puis, quand une machine reconnaît l'empreinte de son propre cluster, une
//! connexion TCP vers le port de contrôle annoncé, où la poignée de main
//! HMAC ([`crate::hmac_auth`]) a lieu.
//!
//! Ce module ne contient que les formats et leur encodage — l'agent (binaire
//! `locaryn-cluster-agent`) tient les sockets. Séparer les deux, c'est ce qui
//! rend le format testable sans réseau.

use crate::catalog::SharedModel;
use crate::sharing::SharingAnnounce;
use crate::Capability;
use serde::{Deserialize, Serialize};

/// Message diffusé en UDP sur [`crate::DISCOVERY_UDP_PORT`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Beacon {
    pub v: u8,
    /// Empreinte du cluster émetteur — voir [`crate::beacon_fingerprint`].
    pub fp: String,
    /// Port TCP où joindre le canal de contrôle de cette machine.
    pub ctrl_port: u16,
}

impl Beacon {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// Décode une balise reçue. `None` sur tout ce qui ne ressemble pas à une
    /// balise de ce protocole — un paquet UDP sur ce port peut venir
    /// d'ailleurs, et ce n'est pas une erreur à faire remonter.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        // Une balise tient en quelques dizaines d'octets ; un paquet
        // beaucoup plus gros n'est pas une balise et ne vaut pas la peine
        // d'être désérialisé.
        if bytes.len() > 512 {
            return None;
        }
        serde_json::from_slice(bytes).ok()
    }
}

/// Après la poignée de main HMAC, chaque partie envoie ses capacités :
/// c'est ce qui remplit [`crate::Peer::capability`] de l'autre côté.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityAnnounce {
    pub v: u8,
    pub capability: Capability,
    /// Port où `ggml-rpc-server` répond, si cette machine l'a démarré.
    pub rpc_port: Option<u16>,
    /// Ce que cette machine prête, et les modèles partagés qu'elle héberge.
    /// Absent chez une machine d'avant le partage : elle reste un pair valable.
    #[serde(default)]
    pub sharing: Option<SharingAnnounce>,
}

/// Une requête sur le canal de contrôle, une fois la poignée de main faite.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlRequest {
    /// « Dis-moi tes capacités actuelles » — utilisé pour rafraîchir la VRAM
    /// libre d'un pair déjà connu sans refaire toute la poignée de main.
    Ping,
    /// « Démarre ton serveur RPC et dis-moi son port » — envoyé par un
    /// coordinateur qui a inclus ce pair dans un plan.
    StartRpc,
    /// « Arrête-le » — après une session, pour ne pas laisser un port RPC
    /// ouvert plus longtemps que nécessaire.
    StopRpc,
    /// « Quels modèles partages-tu ? » — envoyé au coordinateur par un poste
    /// client.
    Catalog,
    /// « Envoie-moi ce modèle à partir de cet octet » — la réponse
    /// [`ControlResponse::ModelStream`] est suivie des octets bruts, sur la
    /// même connexion. Reprendre à `offset` évite de recopier 15 Go après une
    /// coupure à 14.
    FetchModel { file: String, offset: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlResponse {
    Capability(CapabilityAnnounce),
    RpcStarted {
        port: u16,
    },
    RpcStopped,
    Catalog {
        models: Vec<SharedModel>,
    },
    /// En-tête d'un envoi : `size_bytes - offset` octets suivent.
    ModelStream {
        file: String,
        size_bytes: u64,
        offset: u64,
    },
    Error {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capacite() -> Capability {
        Capability {
            name: "essai".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            gpu_name: Some("RTX 4050".into()),
            free_vram_gb: 6.0,
            has_llama_server: true,
            has_rpc_server: true,
            offered_memory_gb: 0.0,
            total_vram_gb: 8.0,
            total_ram_gb: 32.0,
            free_ram_gb: 16.0,
            cpu_usage_percent: 0.0,
        }
    }

    #[test]
    fn une_balise_fait_l_aller_retour() {
        let b = Beacon {
            v: crate::PROTOCOL_VERSION,
            fp: "0123456789abcdef".into(),
            ctrl_port: 41338,
        };
        let bytes = b.encode();
        assert_eq!(Beacon::decode(&bytes), Some(b));
    }

    /// Un paquet UDP sur le même port qui ne parle pas ce protocole ne doit
    /// jamais faire paniquer le décodeur — le port de découverte n'est pas
    /// réservé à cette seule application sur toutes les machines.
    #[test]
    fn un_paquet_etranger_ne_fait_pas_paniquer_le_decodage() {
        assert_eq!(Beacon::decode(b"\x00\x01\x02n'importe quoi"), None);
        assert_eq!(Beacon::decode(&[0u8; 2000]), None);
        assert_eq!(Beacon::decode(b""), None);
    }

    #[test]
    fn une_annonce_de_capacite_conserve_le_port_rpc() {
        let a = CapabilityAnnounce {
            v: 1,
            capability: capacite(),
            rpc_port: Some(50052),
            sharing: None,
        };
        let bytes = serde_json::to_vec(&a).unwrap();
        let relu: CapabilityAnnounce = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(relu.rpc_port, Some(50052));
        assert_eq!(relu.capability.free_vram_gb, 6.0);
    }

    /// Une machine en version 0.1 n'annonce ni partage ni mémoire offerte :
    /// elle doit rester lisible, pas être rejetée du cluster.
    #[test]
    fn l_annonce_d_une_machine_d_avant_le_partage_se_lit_encore() {
        let ancienne = r#"{"v":1,"capability":{"name":"a","os":"linux","arch":"x86_64","gpu_name":null,"free_vram_gb":8.0,"has_llama_server":true,"has_rpc_server":true},"rpc_port":null}"#;
        let relu: CapabilityAnnounce = serde_json::from_str(ancienne).unwrap();
        assert!(relu.sharing.is_none());
        assert_eq!(relu.capability.offered_memory_gb, 0.0);
    }

    #[test]
    fn une_demande_de_modele_porte_son_point_de_reprise() {
        let json = serde_json::to_string(&ControlRequest::FetchModel {
            file: "a.gguf".into(),
            offset: 42,
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"kind":"fetch_model","file":"a.gguf","offset":42}"#
        );
    }

    #[test]
    fn les_requetes_de_controle_s_encodent_avec_leur_etiquette() {
        let json = serde_json::to_string(&ControlRequest::StartRpc).unwrap();
        assert_eq!(json, r#"{"kind":"start_rpc"}"#);
    }
}
