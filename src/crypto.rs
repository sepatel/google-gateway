use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::RngCore;

const NONCE_LEN: usize = 12;

/// Seals secrets at rest as base64(nonce || ciphertext).
#[derive(Clone)]
pub struct Cipher {
    aead: ChaCha20Poly1305,
}

impl Cipher {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            aead: ChaCha20Poly1305::new(Key::from_slice(key)),
        }
    }

    pub fn seal(&self, plaintext: &str) -> Result<String> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let ciphertext = self
            .aead
            .encrypt(Nonce::from_slice(&nonce), plaintext.as_bytes())
            .map_err(|_| anyhow::anyhow!("encryption failed"))?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ciphertext);
        Ok(STANDARD.encode(out))
    }

    pub fn open(&self, sealed: &str) -> Result<String> {
        let bytes = STANDARD
            .decode(sealed)
            .context("sealed value is not base64")?;
        if bytes.len() <= NONCE_LEN {
            anyhow::bail!("sealed value is too short");
        }
        let (nonce, ciphertext) = bytes.split_at(NONCE_LEN);
        let plaintext = self
            .aead
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| {
                anyhow::anyhow!(
                    "cannot decrypt stored secret (wrong GOOGLE_GATEWAY_ENCRYPTION_KEY?)"
                )
            })?;
        String::from_utf8(plaintext).context("decrypted secret is not UTF-8")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_different_key_cannot_open_a_sealed_secret() {
        let sealed = Cipher::new(&[1; 32]).seal("refresh-token").unwrap();
        assert_eq!(
            Cipher::new(&[1; 32]).open(&sealed).unwrap(),
            "refresh-token"
        );
        assert!(Cipher::new(&[2; 32]).open(&sealed).is_err());
    }
}
