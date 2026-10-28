//! API-key hashing.
//!
//! Keys are stored as `HMAC-SHA256(raw_key, pepper)` (hex) rather than in
//! plaintext, so a database read alone cannot recover a key or verify a guess
//! without the server-side pepper (`ROUTER_KEY_PEPPER`). The HMAC is
//! deterministic, so lookup by hash stays O(1) (unlike a salted password hash).
//!
//! The raw secret is shown to the caller exactly once at creation; afterwards
//! only the non-secret [`key_prefix`] is ever returned.
//!
//! Singular deployments carry their own `ROUTER_KEY_PEPPER`. Cluster deployments
//! share one pepper so an opaque key can select its owning environment through
//! an indexed hash lookup. Ambiguous ownership fails closed; tenant stores still
//! filter by environment. Retain the existing pepper when expanding a deployment.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Hashes raw API keys with a server-side pepper.
#[derive(Clone)]
pub struct KeyHasher {
    pepper: Vec<u8>,
}

impl KeyHasher {
    pub fn new(pepper: impl Into<Vec<u8>>) -> Self {
        Self {
            pepper: pepper.into(),
        }
    }

    /// Load the pepper from `ROUTER_KEY_PEPPER`. Returns an error if unset or
    /// empty so we never silently hash with an empty key.
    pub fn from_env() -> Result<Self, String> {
        match std::env::var("ROUTER_KEY_PEPPER") {
            Ok(p) if !p.is_empty() => Ok(Self::new(p.into_bytes())),
            _ => Err("ROUTER_KEY_PEPPER must be set (non-empty) to hash API keys".to_string()),
        }
    }

    /// Hex-encoded HMAC-SHA256 of the raw key. This is the value stored as
    /// `chalk_router.api_keys.key_hash` and used for lookups.
    pub fn hash(&self, raw_key: &str) -> String {
        let mut mac =
            HmacSha256::new_from_slice(&self.pepper).expect("HMAC accepts a key of any length");
        mac.update(raw_key.as_bytes());
        hex_encode(&mac.finalize().into_bytes())
    }

    /// Derive the codec for opaque stateful Responses ids from the same
    /// environment-scoped server secret without exposing the key material.
    pub fn response_locator_codec(&self) -> crate::response_locator::ResponseLocatorCodec {
        crate::response_locator::ResponseLocatorCodec::new(self.pepper.clone())
    }

    /// Derive the HMAC key trace sampling hashes `session.id`/trace ids with
    /// (see `telemetry::sampling::SamplingGate::set_sampling_secret`), keeping
    /// that use domain-separated from `hash` and `response_locator_codec`
    /// rather than reusing the raw pepper bytes directly.
    pub fn sampling_secret(&self) -> Vec<u8> {
        let mut mac =
            HmacSha256::new_from_slice(&self.pepper).expect("HMAC accepts a key of any length");
        mac.update(b"chalk-router:trace-sampling");
        mac.finalize().into_bytes().to_vec()
    }

    /// Tenant-specific sampling decisions retain one provisioned cluster pepper.
    pub fn sampling_secret_for_environment(&self, environment: &str) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(&self.sampling_secret())
            .expect("HMAC accepts any key length");
        mac.update(&(environment.len() as u64).to_be_bytes());
        mac.update(environment.as_bytes());
        mac.finalize().into_bytes().to_vec()
    }
}

/// Non-secret display prefix for a raw key, e.g. `"ck-1a2b3c4d"`. Safe to store
/// and return in list responses; not enough to reconstruct the key.
pub fn key_prefix(raw_key: &str) -> String {
    raw_key.chars().take(11).collect()
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_deterministic_and_pepper_dependent() {
        let a = KeyHasher::new(b"pepper-1".to_vec());
        let b = KeyHasher::new(b"pepper-2".to_vec());
        assert_eq!(a.hash("ck-abc"), a.hash("ck-abc"));
        assert_ne!(
            a.hash("ck-abc"),
            b.hash("ck-abc"),
            "different pepper -> different hash"
        );
        assert_eq!(a.hash("ck-abc").len(), 64, "sha256 hex is 64 chars");
    }

    #[test]
    fn prefix_is_short_and_nonsecret() {
        assert_eq!(key_prefix("ck-0123456789abcdef"), "ck-01234567");
    }
}
