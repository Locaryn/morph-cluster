//! Identité de cluster : nom, secret partagé, code de pairage.
//!
//! Le secret ne voyage jamais sur le réseau en clair — ni dans la balise de
//! découverte (qui ne porte qu'une empreinte, voir [`crate::beacon_fingerprint`]),
//! ni dans la poignée de main (qui ne porte qu'une preuve HMAC, voir
//! [`crate::hmac_auth`]). Il ne se transmet que par le code de pairage, que
//! l'utilisateur copie lui-même d'une machine à l'autre — comme une clé SSH.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// L'identité d'un cluster : ce que chaque machine membre doit connaître pour
/// s'y reconnaître.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterIdentity {
    /// Nom choisi par l'utilisateur à la création (`"bureau"`, `"atelier"`).
    /// Purement décoratif — c'est le secret qui fait l'appartenance.
    pub name: String,
    /// Identifiant stable, aléatoire, généré une fois à la création.
    pub cluster_id: String,
    /// Secret partagé, 32 octets aléatoires. Jamais loggé, jamais renvoyé
    /// tel quel par un outil MCP hors de la création/lecture explicite du
    /// code de pairage.
    pub secret: Vec<u8>,
}

const OCTETS_SECRET: usize = 32;
/// Préfixe du code de pairage — reconnaissable dans un message, un
/// gestionnaire de mots de passe, un historique de terminal.
const PREFIXE_CODE: &str = "locaryn-cluster-v1:";

impl ClusterIdentity {
    /// Crée une nouvelle identité, avec un secret tiré du générateur
    /// cryptographique du système — jamais `rand` applicatif, dont la graine
    /// peut être faible ou partagée entre deux installations.
    pub fn generate(name: &str) -> Self {
        Self {
            name: name.trim().to_string(),
            cluster_id: random_id(),
            secret: random_secret(),
        }
    }

    /// Le code à copier sur une autre machine. Encode le nom, l'identifiant
    /// et le secret en base64 URL-safe sans remplissage — collable dans un
    /// champ texte, un message, un QR code.
    pub fn pairing_code(&self) -> String {
        let brut = format!(
            "{}\u{1}{}\u{1}{}",
            self.name,
            self.cluster_id,
            base64_encode(&self.secret)
        );
        format!("{PREFIXE_CODE}{}", base64_encode(brut.as_bytes()))
    }

    /// Lit un code de pairage. `Err` nomme précisément ce qui ne va pas — un
    /// code tronqué en collant depuis un champ trop étroit est l'erreur la
    /// plus fréquente, et « code invalide » sans plus de détail n'aide pas à
    /// la repérer.
    pub fn from_pairing_code(code: &str) -> Result<Self, String> {
        let code = code.trim();
        let Some(reste) = code.strip_prefix(PREFIXE_CODE) else {
            return Err(format!(
                "ce n'est pas un code de pairage Cluster (attendu : préfixe « {PREFIXE_CODE} »)"
            ));
        };
        let brut = base64_decode(reste)
            .map_err(|e| format!("code de pairage tronqué ou corrompu : {e}"))?;
        let texte = String::from_utf8(brut)
            .map_err(|_| "code de pairage corrompu : contenu non textuel".to_string())?;
        let mut parts = texte.split('\u{1}');
        let (Some(name), Some(cluster_id), Some(secret_b64)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return Err("code de pairage incomplet".to_string());
        };
        if parts.next().is_some() {
            return Err("code de pairage mal formé (trop de champs)".to_string());
        }
        let secret = base64_decode(secret_b64).map_err(|e| format!("secret illisible : {e}"))?;
        if secret.len() != OCTETS_SECRET {
            return Err(format!(
                "secret de longueur inattendue ({} octets, {OCTETS_SECRET} attendus) — le code a \
                 probablement été tronqué en le copiant",
                secret.len()
            ));
        }
        if cluster_id.trim().is_empty() {
            return Err("identifiant de cluster vide dans le code".to_string());
        }
        Ok(Self {
            name: name.to_string(),
            cluster_id: cluster_id.to_string(),
            secret,
        })
    }

    /// Écrit l'identité dans le dossier d'état. Permissions restreintes sous
    /// Unix : ce fichier contient le secret du cluster.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("identity.json");
        let json = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        std::fs::write(&path, json)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    pub fn load(dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(dir.join("identity.json")).ok()?;
        serde_json::from_str(&text).ok()
    }
}

fn random_id() -> String {
    hex_encode(&random_bytes(16))
}

fn random_secret() -> Vec<u8> {
    random_bytes(OCTETS_SECRET)
}

/// Octets aléatoires depuis la source cryptographique du système
/// (`/dev/urandom`, `BCryptGenRandom`…) via `getrandom` : une seule fonction
/// sûre, déjà auditée, plutôt qu'un appel système écrit à la main pour
/// chaque plateforme.
fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).expect("source aléatoire du système indisponible");
    buf
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
        out.push(ALPHABET[(n >> 18 & 0x3f) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6 & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3f) as usize] as char);
        }
    }
    out
}

fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut table = [255u8; 256];
    for (i, &c) in ALPHABET.iter().enumerate() {
        table[c as usize] = i as u8;
    }
    let s = s.trim();
    let mut out = Vec::with_capacity(s.len() * 3 / 4 + 3);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        let v = table[c as usize];
        if v == 255 {
            return Err(format!("caractère hors alphabet : {}", c as char));
        }
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_code_de_pairage_fait_l_aller_retour() {
        let id = ClusterIdentity::generate("atelier");
        let code = id.pairing_code();
        assert!(code.starts_with(PREFIXE_CODE));
        let relu = ClusterIdentity::from_pairing_code(&code).unwrap();
        assert_eq!(relu, id);
    }

    #[test]
    fn deux_identites_ont_des_secrets_differents() {
        let a = ClusterIdentity::generate("x");
        let b = ClusterIdentity::generate("x");
        assert_ne!(a.secret, b.secret);
        assert_ne!(a.cluster_id, b.cluster_id);
    }

    #[test]
    fn le_secret_ne_traine_pas_en_clair_dans_le_code() {
        let id = ClusterIdentity::generate("atelier");
        let code = id.pairing_code();
        // Le secret encodé brut ne doit pas apparaître tel quel : il passe
        // par un second encodage, pas une simple concaténation.
        let secret_seul = base64_encode(&id.secret);
        assert!(!code.contains(&secret_seul) || secret_seul.len() < 4);
    }

    #[test]
    fn un_code_sans_prefixe_est_refuse_clairement() {
        let err = ClusterIdentity::from_pairing_code("n-importe-quoi").unwrap_err();
        assert!(err.contains("préfixe"));
    }

    #[test]
    fn un_code_tronque_est_signale_comme_tel() {
        let id = ClusterIdentity::generate("atelier");
        let code = id.pairing_code();
        let tronque = &code[..code.len() - 10];
        let err = ClusterIdentity::from_pairing_code(tronque).unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn base64_maison_fait_l_aller_retour_sur_toutes_les_longueurs() {
        for n in 0..40 {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 % 251) as u8).collect();
            let encoded = base64_encode(&data);
            let decoded = base64_decode(&encoded).unwrap();
            assert_eq!(decoded, data, "échec pour n={n}");
        }
    }

    #[test]
    fn sauvegarde_puis_lecture_redonne_la_meme_identite() {
        let dir = std::env::temp_dir().join(format!(
            "locaryn_cluster_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let id = ClusterIdentity::generate("essai");
        id.save(&dir).unwrap();
        let relu = ClusterIdentity::load(&dir).unwrap();
        assert_eq!(id, relu);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
