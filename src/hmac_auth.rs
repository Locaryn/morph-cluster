//! Poignée de main d'authentification mutuelle entre deux machines du même
//! cluster.
//!
//! Ni l'une ni l'autre ne transmet le secret : chacune prouve qu'elle le
//! connaît en produisant un HMAC-SHA256 sur un nonce fourni par l'autre. Un
//! observateur du réseau qui capture l'échange entier n'apprend jamais le
//! secret lui-même — il ne peut pas non plus rejouer l'échange, puisque le
//! nonce change à chaque tentative.
//!
//! Ce module ne fait que le calcul ; le canal (TCP, dans [`crate::discovery`])
//! est ailleurs. Toute la logique qui décide qui a raison est ici, testable
//! sans ouvrir un seul socket.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Longueur du nonce, en octets. Assez pour qu'une collision ou une
/// devinette soit hors de portée sur la durée d'une poignée de main.
pub const NONCE_LEN: usize = 32;

/// Message envoyé en premier par la machine qui initie le contact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub v: u8,
    /// Nonce choisi par l'initiateur, que l'autre partie doit signer.
    pub nonce: String,
}

/// Réponse : la preuve que le nonce de l'initiateur a été signée, plus un
/// nonce à signer en retour — la poignée de main se referme dans le message
/// suivant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloAck {
    pub v: u8,
    pub proof: String,
    pub nonce: String,
}

/// Dernier message : la preuve du nonce de `HelloAck`. Après ce message, les
/// deux parties savent que l'autre connaît le secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Confirm {
    pub v: u8,
    pub proof: String,
}

/// Tire un nonce aléatoire, encodé en hexadécimal pour transiter sans souci
/// d'échappement dans du JSON.
pub fn generate_nonce() -> String {
    let mut buf = [0u8; NONCE_LEN];
    getrandom::fill(&mut buf).expect("source aléatoire du système indisponible");
    hex_encode(&buf)
}

/// Calcule la preuve HMAC-SHA256(secret, nonce), en hexadécimal.
pub fn prove(secret: &[u8], nonce: &str) -> String {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(secret).expect("clé HMAC de taille libre");
    mac.update(nonce.as_bytes());
    hex_encode(&mac.finalize().into_bytes())
}

/// Vérifie une preuve en temps constant : comparer octet à octet sans sortir
/// dès le premier écart, pour qu'un aller-retour chronométré ne renseigne pas
/// un attaquant sur combien d'octets étaient déjà bons.
pub fn verify(secret: &[u8], nonce: &str, proof_hex: &str) -> bool {
    let attendu = prove(secret, nonce);
    if attendu.len() != proof_hex.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in attendu.bytes().zip(proof_hex.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Où en est une poignée de main côté initiateur.
pub struct InitiatorHandshake {
    secret: Vec<u8>,
    nonce_envoye: String,
}

impl InitiatorHandshake {
    /// Démarre la poignée de main : produit le premier message à envoyer.
    pub fn start(secret: &[u8]) -> (Self, Hello) {
        let nonce = generate_nonce();
        (
            Self {
                secret: secret.to_vec(),
                nonce_envoye: nonce.clone(),
            },
            Hello {
                v: crate::PROTOCOL_VERSION,
                nonce,
            },
        )
    }

    /// Reçoit la réponse de l'autre partie. Vérifie sa preuve, et si elle est
    /// bonne, produit la confirmation à renvoyer — l'autre partie est alors
    /// authentifiée de ce côté-ci.
    pub fn receive_ack(&self, ack: &HelloAck) -> Result<Confirm, String> {
        if !verify(&self.secret, &self.nonce_envoye, &ack.proof) {
            return Err(
                "l'autre machine n'a pas produit la bonne preuve — secret différent, ou message \
                 altéré en chemin"
                    .to_string(),
            );
        }
        Ok(Confirm {
            v: crate::PROTOCOL_VERSION,
            proof: prove(&self.secret, &ack.nonce),
        })
    }
}

/// Où en est une poignée de main côté répondant.
pub struct ResponderHandshake {
    secret: Vec<u8>,
    nonce_envoye: String,
}

impl ResponderHandshake {
    /// Reçoit le premier message, produit la réponse (preuve + nonce
    /// propre).
    pub fn respond(secret: &[u8], hello: &Hello) -> (Self, HelloAck) {
        let nonce = generate_nonce();
        (
            Self {
                secret: secret.to_vec(),
                nonce_envoye: nonce.clone(),
            },
            HelloAck {
                v: crate::PROTOCOL_VERSION,
                proof: prove(secret, &hello.nonce),
                nonce,
            },
        )
    }

    /// Reçoit la confirmation finale. `Ok` signifie : l'initiateur a prouvé
    /// qu'il connaît le secret — l'authentification est mutuelle et
    /// terminée.
    pub fn receive_confirm(&self, confirm: &Confirm) -> Result<(), String> {
        if verify(&self.secret, &self.nonce_envoye, &confirm.proof) {
            Ok(())
        } else {
            Err("preuve finale invalide — poignée de main refusée".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn une_poignee_de_main_avec_le_meme_secret_reussit_des_deux_cotes() {
        let secret = b"secret-partage".to_vec();

        let (initiateur, hello) = InitiatorHandshake::start(&secret);
        let (repondant, ack) = ResponderHandshake::respond(&secret, &hello);
        let confirm = initiateur
            .receive_ack(&ack)
            .expect("preuve du répondant valide");
        repondant
            .receive_confirm(&confirm)
            .expect("preuve de l'initiateur valide");
    }

    #[test]
    fn un_secret_different_fait_echouer_la_poignee_de_main() {
        let secret_a = b"secret-a".to_vec();
        let secret_b = b"secret-b".to_vec();

        let (initiateur, hello) = InitiatorHandshake::start(&secret_a);
        let (_repondant, ack) = ResponderHandshake::respond(&secret_b, &hello);
        let err = initiateur.receive_ack(&ack).unwrap_err();
        assert!(!err.is_empty());
    }

    /// Le répondant peut avoir raison sur le premier tour (même secret) et
    /// pourtant l'initiateur mentir sur le second — les deux preuves doivent
    /// être vérifiées, pas seulement la première.
    #[test]
    fn une_confirmation_falsifiee_est_rejetee() {
        let secret = b"secret-partage".to_vec();
        let (_initiateur, hello) = InitiatorHandshake::start(&secret);
        let (repondant, _ack) = ResponderHandshake::respond(&secret, &hello);

        let fausse_confirmation = Confirm {
            v: crate::PROTOCOL_VERSION,
            proof: "0000000000000000000000000000000000000000000000000000000000000000".into(),
        };
        assert!(repondant.receive_confirm(&fausse_confirmation).is_err());
    }

    #[test]
    fn deux_nonces_generes_sont_differents() {
        assert_ne!(generate_nonce(), generate_nonce());
    }

    #[test]
    fn verify_refuse_une_preuve_de_mauvaise_longueur() {
        assert!(!verify(b"secret", "nonce", "abc"));
    }
}
