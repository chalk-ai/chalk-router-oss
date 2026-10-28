//! Symmetric encryption for at-rest provider credentials in Postgres.
//!
//! Provider API keys must be replayed verbatim to upstream providers
//! (`Authorization: Bearer …` / `x-api-key`), so they cannot be hashed — they
//! are encrypted at rest instead. The KEK is loaded from `ROUTER_SECRETS_KEK`
//! (base64-encoded 32 bytes); each secret is stored as a self-describing
//! versioned envelope — `[format_version: 1B][nonce: 12B][ciphertext + GCM tag]`
//! — encrypted with AES-256-GCM under a fresh 12-byte nonce.
//!
//! Custody mirrors [`crate::hashing::KeyHasher`]'s pepper: the KEK rides the
//! same deployment secret channel (SOPS → GCP KMS → k8s secret) and is
//! required whenever a database is configured. Losing it does not lose the
//! keys irrecoverably — they can be re-entered via the Providers UI.
//! Cluster deployments share one KEK across their environment-filtered stores.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine;
use rand::RngCore;

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
/// AES-256-GCM authentication tag length, appended to the ciphertext by the AEAD.
const TAG_LEN: usize = 16;
/// Current envelope layout: `[FORMAT_VERSION][nonce][ciphertext + tag]`. Bump
/// when the layout or scheme changes; [`SecretCipher::decrypt`] rejects any
/// version it does not recognize. The version byte is framing only (it is NOT
/// fed to the AEAD as AAD) so an envelope repacked from a pre-envelope
/// `(nonce, ciphertext)` pair — same nonce, same ciphertext bytes — still
/// decrypts unchanged.
const FORMAT_VERSION: u8 = 1;

/// A stored envelope could not be decrypted: wrong/rotated `ROUTER_SECRETS_KEK`,
/// an unsupported format version, a malformed layout, or corruption. Surfaced per-row as
/// [`crate::provider_connections::StoredConnection::Unreadable`] rather than
/// conflated with an absent connection.
#[derive(Debug, Clone)]
pub struct DecryptError(pub String);

impl std::fmt::Display for DecryptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for DecryptError {}

/// AES-256-GCM cipher over a single key-encryption key.
#[derive(Clone)]
pub struct SecretCipher {
    cipher: Aes256Gcm,
}

impl SecretCipher {
    /// Load the KEK from `ROUTER_SECRETS_KEK` (base64, 32 bytes after decode).
    /// Errors if unset or empty so we never encrypt under an empty key — the
    /// same fail-fast posture as [`crate::hashing::KeyHasher::from_env`].
    pub fn from_env() -> Result<Self, String> {
        match std::env::var("ROUTER_SECRETS_KEK") {
            Ok(raw) if !raw.is_empty() => Self::from_base64(&raw),
            _ => Err(
                "ROUTER_SECRETS_KEK must be set (non-empty) to encrypt provider \
                      credentials; generate one with `openssl rand -base64 32`"
                    .to_string(),
            ),
        }
    }

    pub fn from_base64(value: &str) -> Result<Self, String> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(value.trim())
            .map_err(|e| format!("ROUTER_SECRETS_KEK base64 decode: {e}"))?;
        if bytes.len() != KEY_LEN {
            return Err(format!(
                "ROUTER_SECRETS_KEK must decode to {KEY_LEN} bytes, got {}",
                bytes.len()
            ));
        }
        let key = Key::<Aes256Gcm>::from_slice(&bytes);
        Ok(Self {
            cipher: Aes256Gcm::new(key),
        })
    }

    /// Encrypt `plaintext` into a versioned envelope
    /// (`[FORMAT_VERSION][nonce: 12B][ciphertext + tag]`) for the single
    /// `provider_connections.api_key_envelope` column.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, String> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = self
            .cipher
            .encrypt(nonce, plaintext)
            .map_err(|e| format!("encrypt: {e}"))?;
        let mut envelope = Vec::with_capacity(1 + NONCE_LEN + ciphertext.len());
        envelope.push(FORMAT_VERSION);
        envelope.extend_from_slice(&nonce_bytes);
        envelope.extend_from_slice(&ciphertext);
        Ok(envelope)
    }

    /// Decrypt an envelope produced by [`Self::encrypt`]. Validates the format
    /// version and minimum length before handing the ciphertext to AES-GCM.
    pub fn decrypt(&self, envelope: &[u8]) -> Result<Vec<u8>, DecryptError> {
        if envelope.len() < 1 + NONCE_LEN + TAG_LEN {
            return Err(DecryptError(format!(
                "envelope too short: {} bytes (need >= {})",
                envelope.len(),
                1 + NONCE_LEN + TAG_LEN
            )));
        }
        let version = envelope[0];
        if version != FORMAT_VERSION {
            return Err(DecryptError(format!(
                "unsupported envelope version {version}"
            )));
        }
        let nonce = Nonce::from_slice(&envelope[1..1 + NONCE_LEN]);
        let ciphertext = &envelope[1 + NONCE_LEN..];
        self.cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| DecryptError(format!("decrypt: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn random_kek_b64() -> String {
        let mut bytes = [0u8; KEY_LEN];
        rand::thread_rng().fill_bytes(&mut bytes);
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn roundtrip() {
        let cipher = SecretCipher::from_base64(&random_kek_b64()).unwrap();
        let pt = b"sk-secret-provider-key";
        let envelope = cipher.encrypt(pt).unwrap();
        assert_eq!(envelope[0], FORMAT_VERSION, "leading format version byte");
        assert_eq!(
            envelope.len(),
            1 + NONCE_LEN + pt.len() + TAG_LEN,
            "version + nonce + ciphertext + tag"
        );
        assert_ne!(
            &envelope[1 + NONCE_LEN..],
            pt.as_slice(),
            "ciphertext != plaintext"
        );
        assert_eq!(cipher.decrypt(&envelope).unwrap(), pt);
    }

    #[test]
    fn wrong_key_fails_to_decrypt() {
        let a = SecretCipher::from_base64(&random_kek_b64()).unwrap();
        let b = SecretCipher::from_base64(&random_kek_b64()).unwrap();
        let envelope = a.encrypt(b"hello").unwrap();
        assert!(b.decrypt(&envelope).is_err(), "wrong KEK must not decrypt");
    }

    #[test]
    fn rejects_wrong_key_length() {
        let bad = base64::engine::general_purpose::STANDARD.encode([0u8; 16]);
        assert!(SecretCipher::from_base64(&bad).is_err());
    }

    #[test]
    fn rejects_short_envelope() {
        let cipher = SecretCipher::from_base64(&random_kek_b64()).unwrap();
        assert!(
            cipher.decrypt(&[FORMAT_VERSION; 4]).is_err(),
            "too short to hold nonce + tag"
        );
    }

    #[test]
    fn rejects_unknown_version() {
        let cipher = SecretCipher::from_base64(&random_kek_b64()).unwrap();
        let mut envelope = cipher.encrypt(b"hello").unwrap();
        envelope[0] = FORMAT_VERSION + 1;
        assert!(
            cipher.decrypt(&envelope).is_err(),
            "unsupported version must be rejected"
        );
    }
}
