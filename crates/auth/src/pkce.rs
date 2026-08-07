//! PKCE (RFC 7636) verifier/challenge generation.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// 32 random octets → 43-char verifier, S256 challenge.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("os rng");
        let verifier = URL_SAFE_NO_PAD.encode(bytes);
        let challenge = challenge_s256(&verifier);
        Self { verifier, challenge }
    }
}

pub fn challenge_s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc7636_test_vector() {
        // Appendix B of RFC 7636.
        assert_eq!(
            challenge_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifier_shape() {
        let p = Pkce::generate();
        assert_eq!(p.verifier.len(), 43);
        assert!(
            p.verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_eq!(p.challenge, challenge_s256(&p.verifier));
        assert_ne!(Pkce::generate().verifier, p.verifier);
    }
}
