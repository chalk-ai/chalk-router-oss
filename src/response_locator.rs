//! Integrity-protected routing locators for stateful Responses operations.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

const PREFIX: &str = "resp_chalk_1";
type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseLocator {
    #[serde(rename = "v")]
    version: u8,
    #[serde(rename = "c")]
    pub connection_id: String,
    #[serde(rename = "r")]
    pub upstream_response_id: String,
    #[serde(default, rename = "e", skip_serializing_if = "Option::is_none")]
    pub environment_id: Option<String>,
    /// The model route the response was created through, so a key granted
    /// that route may retrieve it. Absent for direct model ids and for
    /// locators minted before route grants.
    #[serde(default, rename = "m", skip_serializing_if = "Option::is_none")]
    pub model_route_id: Option<String>,
}

#[derive(Clone)]
pub struct ResponseLocatorCodec {
    key: std::sync::Arc<[u8]>,
    environment_id: Option<String>,
    accept_legacy: bool,
}

impl ResponseLocatorCodec {
    pub fn new(key: impl Into<Vec<u8>>) -> Self {
        Self {
            key: std::sync::Arc::from(key.into()),
            environment_id: None,
            accept_legacy: false,
        }
    }

    /// Reuse cluster signing material, confining legacy locators to their original tenant.
    pub fn for_environment(&self, environment_id: &str, accept_legacy: bool) -> Self {
        Self {
            key: self.key.clone(),
            environment_id: Some(environment_id.into()),
            accept_legacy,
        }
    }

    pub fn encode(
        &self,
        connection_id: &str,
        model_route_id: Option<&str>,
        upstream_response_id: &str,
    ) -> String {
        let payload = serde_json::to_vec(&ResponseLocator {
            version: 1,
            connection_id: connection_id.to_string(),
            upstream_response_id: upstream_response_id.to_string(),
            environment_id: self.environment_id.clone(),
            model_route_id: model_route_id.map(str::to_string),
        })
        .expect("response locator serializes");
        let signature = self.sign(&payload);
        format!(
            "{PREFIX}.{}.{}",
            URL_SAFE_NO_PAD.encode(payload),
            URL_SAFE_NO_PAD.encode(signature)
        )
    }

    pub fn is_environment_bound(&self) -> bool {
        self.environment_id.is_some()
    }

    pub fn decode(&self, encoded: &str) -> Result<ResponseLocator, ResponseLocatorError> {
        let mut parts = encoded.split('.');
        if parts.next() != Some(PREFIX) {
            return Err(ResponseLocatorError::NotRouterLocator);
        }
        let payload = parts.next().ok_or(ResponseLocatorError::Malformed)?;
        let signature = parts.next().ok_or(ResponseLocatorError::Malformed)?;
        if parts.next().is_some() {
            return Err(ResponseLocatorError::Malformed);
        }
        let payload = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| ResponseLocatorError::Malformed)?;
        let signature = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| ResponseLocatorError::Malformed)?;
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(&payload);
        mac.verify_slice(&signature)
            .map_err(|_| ResponseLocatorError::InvalidSignature)?;
        let locator: ResponseLocator =
            serde_json::from_slice(&payload).map_err(|_| ResponseLocatorError::Malformed)?;
        if locator.version != 1
            || locator.connection_id.is_empty()
            || locator.upstream_response_id.is_empty()
        {
            return Err(ResponseLocatorError::Malformed);
        }
        if locator.environment_id != self.environment_id
            && !(locator.environment_id.is_none() && self.accept_legacy)
        {
            return Err(ResponseLocatorError::InvalidSignature);
        }
        Ok(locator)
    }

    pub fn is_router_locator(value: &str) -> bool {
        value.starts_with(&format!("{PREFIX}."))
    }

    fn sign(&self, payload: &[u8]) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(payload);
        mac.finalize().into_bytes().to_vec()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseLocatorError {
    NotRouterLocator,
    Malformed,
    InvalidSignature,
}

impl std::fmt::Display for ResponseLocatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRouterLocator => f.write_str("response id is not a Chalk response locator"),
            Self::Malformed => f.write_str("response locator is malformed"),
            Self::InvalidSignature => f.write_str("response locator signature is invalid"),
        }
    }
}

impl std::error::Error for ResponseLocatorError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_signing_key_cannot_cross_environments_or_downgrade_legacy_scope() {
        let root = ResponseLocatorCodec::new(b"existing-argo-pepper".to_vec());
        let a = root.for_environment("a", true);
        let b = root.for_environment("b", false);
        let legacy = root.encode("same-connection", None, "same-response");
        assert!(a.decode(&legacy).is_ok());
        assert!(b.decode(&legacy).is_err());
        let locator_a = a.encode("same-connection", None, "same-response");
        let locator_b = b.encode("same-connection", None, "same-response");
        assert!(a.decode(&locator_a).is_ok());
        assert!(b.decode(&locator_b).is_ok());
        assert!(a.decode(&locator_b).is_err());
        assert!(b.decode(&locator_a).is_err());
        assert!(root.decode(&locator_b).is_err());
    }

    #[test]
    fn roundtrips_and_rejects_tampering() {
        let codec = ResponseLocatorCodec::new(b"secret".to_vec());
        let encoded = codec.encode("connection-1", None, "resp_upstream");
        assert_eq!(
            codec.decode(&encoded).unwrap(),
            ResponseLocator {
                version: 1,
                connection_id: "connection-1".to_string(),
                upstream_response_id: "resp_upstream".to_string(),
                environment_id: None,
                model_route_id: None,
            }
        );
        let routed = codec.encode("connection-1", Some("route-1"), "resp_upstream");
        assert_eq!(
            codec.decode(&routed).unwrap().model_route_id.as_deref(),
            Some("route-1")
        );
        let mut tampered = encoded.into_bytes();
        let last = tampered.len() - 1;
        tampered[last] = if tampered[last] == b'A' { b'B' } else { b'A' };
        assert!(matches!(
            codec.decode(std::str::from_utf8(&tampered).unwrap()),
            Err(ResponseLocatorError::InvalidSignature)
        ));
    }
}
