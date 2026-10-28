//! Federated JWT validation against trusted external issuers.
//!
//! Extracted from chalk-mcp-gateway's `src/auth/oidc.rs` so that any Chalk
//! service (mcp-gateway, chalk-router, ...) can accept JWTs minted by Chalk's
//! metadata API server — or any other configured issuer — without sharing a
//! secret with it.
//!
//! The flow:
//! 1. Peek at the (unverified) `iss` claim of an incoming token.
//! 2. If it matches a configured [`IssuerConfig`], resolve that issuer's
//!    signing key — a shared secret from an env var (HS256), a static JWKS
//!    URI, or OIDC discovery (`{issuer}/.well-known/openid-configuration`) —
//!    and fully validate the token (signature, `exp`, optional `aud`).
//! 3. Return normalized [`Claims`]; the caller decides what the identity may
//!    do. A non-matching `iss` returns `None` so the caller can fall back to
//!    its own auth scheme (gateway JWT secret, admin key, API keys, ...).
//!
//! Chalk's metadata API serves OIDC discovery and a JWKS endpoint, so
//! federating with it only requires `IssuerConfig::discovery("https://api.chalk.ai")`
//! (signing keys rotate; never pin them).

use std::collections::HashMap;

use base64::Engine;
use jsonwebtoken::{decode, decode_header, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Misconfiguration detected while building the registry.
    #[error("config error: {0}")]
    Config(String),
    /// Token failed validation (bad signature, claims, or key resolution).
    #[error("auth error: {0}")]
    Auth(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Identity normalized from a validated federated token.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Claims {
    /// Subject of the token, mapped from the issuer's configured subject
    /// claim (default `sub`; Chalk tokens carry the user id there).
    pub sub: String,
    /// Expiry as a unix timestamp; `0` when the issuer does not require one.
    pub exp: usize,
    /// Scopes/permissions, mapped from the issuer's configured scopes claim.
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// Trusted external JWT issuer. Tokens whose `iss` claim matches `issuer` are
/// validated against the issuer's keys (JWKS or shared secret).
///
/// The serde shape is kept identical to chalk-mcp-gateway's historical
/// `[[oidc_issuers]]` TOML sections so existing gateway configs keep parsing.
#[derive(Debug, Clone, Deserialize)]
pub struct IssuerConfig {
    /// The `iss` claim value to match in incoming JWTs.
    pub issuer: String,
    /// JWKS endpoint for asymmetric key validation (RS256, ES256, etc.).
    /// Mutually exclusive with `secret_env`.
    #[serde(default)]
    pub jwks_uri: Option<String>,
    /// OIDC discovery URL. When set, `{discovery_url}/.well-known/openid-configuration`
    /// is fetched to find `jwks_uri`. When omitted and `jwks_uri` is also
    /// omitted, auto-discovery uses the issuer itself if it is a URL.
    #[serde(default)]
    pub discovery_url: Option<String>,
    /// Environment variable holding the shared secret for HS256 validation.
    /// Mutually exclusive with `jwks_uri`.
    #[serde(default)]
    pub secret_env: Option<String>,
    /// Optional `aud` claim validation.
    #[serde(default)]
    pub audience: Option<String>,
    /// Maps normalized claim names to token claim names. Supported keys:
    ///   - `sub`    — which token claim becomes the identity (default: "sub")
    ///   - `scopes` — which token claim supplies scopes (default: "scopes")
    #[serde(default)]
    pub claims_map: HashMap<String, String>,
    /// Whether to require the `exp` claim. Defaults to true.
    #[serde(default = "default_true")]
    pub require_exp: bool,
}

fn default_true() -> bool {
    true
}

impl IssuerConfig {
    /// Issuer validated via OIDC discovery on the issuer URL itself — the
    /// right constructor for Chalk's metadata API server.
    pub fn discovery(issuer: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            jwks_uri: None,
            discovery_url: None,
            secret_env: None,
            audience: None,
            claims_map: HashMap::new(),
            require_exp: true,
        }
    }

    pub fn with_audience(mut self, audience: impl Into<String>) -> Self {
        self.audience = Some(audience.into());
        self
    }
}

enum KeySource {
    Secret(String),
    Jwks { uri: String },
    Discovery { issuer_url: String },
}

struct ResolvedIssuer {
    key_source: KeySource,
    audience: Option<String>,
    subject_claim: String,
    scopes_claim: String,
    require_exp: bool,
}

struct CachedJwks {
    keys: jsonwebtoken::jwk::JwkSet,
    fetched_at: std::time::Instant,
}

/// Registry of trusted issuers with cached JWKS/discovery state.
///
/// Construct once at startup and share (it is internally synchronized);
/// rebuilding per request would defeat the JWKS cache.
pub struct OidcRegistry {
    issuers: HashMap<String, ResolvedIssuer>,
    jwks_cache: RwLock<HashMap<String, CachedJwks>>,
    discovered_uris: RwLock<HashMap<String, String>>,
    http: reqwest::Client,
}

const JWKS_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

impl OidcRegistry {
    /// Registry that trusts no issuers; `try_validate` always returns `None`.
    pub fn empty() -> Self {
        Self::from_config(&[]).expect("empty registry cannot fail")
    }

    pub fn from_config(configs: &[IssuerConfig]) -> Result<Self> {
        let mut issuers = HashMap::new();
        for c in configs {
            let key_source = if let Some(ref env_name) = c.secret_env {
                let secret = std::env::var(env_name).map_err(|_| {
                    Error::Config(format!(
                        "issuer '{}': env var '{env_name}' not set",
                        c.issuer
                    ))
                })?;
                KeySource::Secret(secret)
            } else if let Some(ref uri) = c.jwks_uri {
                KeySource::Jwks { uri: uri.clone() }
            } else if let Some(ref url) = c.discovery_url {
                KeySource::Discovery {
                    issuer_url: url.clone(),
                }
            } else if c.issuer.starts_with("http://") || c.issuer.starts_with("https://") {
                KeySource::Discovery {
                    issuer_url: c.issuer.clone(),
                }
            } else {
                return Err(Error::Config(format!(
                    "issuer '{}': set jwks_uri, secret_env, or discovery_url \
                     (or use an https:// issuer for auto-discovery)",
                    c.issuer
                )));
            };

            let subject_claim = c
                .claims_map
                .get("sub")
                .cloned()
                .unwrap_or_else(|| "sub".to_string());
            let scopes_claim = c
                .claims_map
                .get("scopes")
                .cloned()
                .unwrap_or_else(|| "scopes".to_string());

            issuers.insert(
                c.issuer.clone(),
                ResolvedIssuer {
                    key_source,
                    audience: c.audience.clone(),
                    subject_claim,
                    scopes_claim,
                    require_exp: c.require_exp,
                },
            );
        }
        Ok(Self {
            issuers,
            jwks_cache: RwLock::new(HashMap::new()),
            discovered_uris: RwLock::new(HashMap::new()),
            http: reqwest::Client::new(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.issuers.is_empty()
    }

    /// Try to validate a token against configured federated issuers.
    /// Returns `None` if the token's `iss` claim doesn't match any configured
    /// issuer — the caller should fall back to its own auth scheme.
    pub async fn try_validate(&self, token: &str) -> Option<Result<Claims>> {
        if self.issuers.is_empty() {
            return None;
        }
        let iss = peek_issuer(token)?;
        let issuer = self.issuers.get(&iss)?;
        Some(self.validate_with_issuer(token, &iss, issuer).await)
    }

    async fn validate_with_issuer(
        &self,
        token: &str,
        iss: &str,
        issuer: &ResolvedIssuer,
    ) -> Result<Claims> {
        let header =
            decode_header(token).map_err(|e| Error::Auth(format!("invalid token header: {e}")))?;

        let key = self.resolve_key(issuer, &header).await?;

        let mut validation = Validation::new(header.alg);
        validation.validate_exp = issuer.require_exp;
        if !issuer.require_exp {
            // jsonwebtoken's default `required_spec_claims` demands that `exp`
            // be *present* even when expiry checking is off; drop it so
            // `require_exp = false` genuinely permits tokens without one.
            validation.required_spec_claims.remove("exp");
        }
        if let Some(ref aud) = issuer.audience {
            validation.set_audience(&[aud]);
        } else {
            validation.validate_aud = false;
        }
        validation.set_issuer(&[iss]);

        let token_data = decode::<serde_json::Value>(token, &key, &validation)
            .map_err(|e| Error::Auth(format!("federated token invalid: {e}")))?;

        let sub = token_data
            .claims
            .get(&issuer.subject_claim)
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                Error::Auth(format!(
                    "token missing '{}' claim (mapped from sub)",
                    issuer.subject_claim
                ))
            })?
            .to_string();

        let exp = token_data
            .claims
            .get("exp")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        let scopes = token_data
            .claims
            .get(&issuer.scopes_claim)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        Ok(Claims { sub, exp, scopes })
    }

    async fn resolve_key(
        &self,
        issuer: &ResolvedIssuer,
        header: &jsonwebtoken::Header,
    ) -> Result<DecodingKey> {
        match &issuer.key_source {
            KeySource::Secret(secret) => Ok(DecodingKey::from_secret(secret.as_bytes())),
            KeySource::Jwks { uri } => self.resolve_jwks_key(uri, header).await,
            KeySource::Discovery { issuer_url } => {
                let jwks_uri = self.discover_jwks_uri(issuer_url).await?;
                self.resolve_jwks_key(&jwks_uri, header).await
            }
        }
    }

    async fn resolve_jwks_key(
        &self,
        jwks_uri: &str,
        header: &jsonwebtoken::Header,
    ) -> Result<DecodingKey> {
        let kid = header.kid.as_deref();

        // Try the cache first.
        {
            let cache = self.jwks_cache.read().await;
            if let Some(cached) = cache.get(jwks_uri) {
                if cached.fetched_at.elapsed() < JWKS_CACHE_TTL {
                    if let Some(jwk) = find_jwk(&cached.keys, kid) {
                        return DecodingKey::from_jwk(jwk)
                            .map_err(|e| Error::Auth(format!("invalid JWK: {e}")));
                    }
                    // kid not found in cached set — fall through to refresh,
                    // which covers key rotation within the TTL window.
                }
            }
        }

        let jwks = self.fetch_jwks(jwks_uri).await?;
        let jwk = find_jwk(&jwks, kid).ok_or_else(|| {
            Error::Auth(format!("no matching JWK found (kid={kid:?}) at {jwks_uri}"))
        })?;
        let key =
            DecodingKey::from_jwk(jwk).map_err(|e| Error::Auth(format!("invalid JWK: {e}")))?;

        let mut cache = self.jwks_cache.write().await;
        cache.insert(
            jwks_uri.to_string(),
            CachedJwks {
                keys: jwks,
                fetched_at: std::time::Instant::now(),
            },
        );

        Ok(key)
    }

    async fn discover_jwks_uri(&self, issuer_url: &str) -> Result<String> {
        {
            let cache = self.discovered_uris.read().await;
            if let Some(uri) = cache.get(issuer_url) {
                return Ok(uri.clone());
            }
        }

        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            issuer_url.trim_end_matches('/')
        );
        tracing::info!(%discovery_url, "performing OIDC discovery");

        let resp =
            self.http.get(&discovery_url).send().await.map_err(|e| {
                Error::Auth(format!("OIDC discovery from {discovery_url} failed: {e}"))
            })?;
        let doc: serde_json::Value = resp.json().await.map_err(|e| {
            Error::Auth(format!(
                "OIDC discovery parse from {discovery_url} failed: {e}"
            ))
        })?;

        let jwks_uri = doc
            .get("jwks_uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                Error::Auth(format!("OIDC discovery at {discovery_url} has no jwks_uri"))
            })?
            .to_string();

        tracing::info!(%jwks_uri, "discovered JWKS endpoint");
        let mut cache = self.discovered_uris.write().await;
        cache.insert(issuer_url.to_string(), jwks_uri.clone());

        Ok(jwks_uri)
    }

    async fn fetch_jwks(&self, uri: &str) -> Result<jsonwebtoken::jwk::JwkSet> {
        tracing::debug!(uri, "fetching JWKS");
        let resp = self
            .http
            .get(uri)
            .send()
            .await
            .map_err(|e| Error::Auth(format!("JWKS fetch from {uri} failed: {e}")))?;
        resp.json()
            .await
            .map_err(|e| Error::Auth(format!("JWKS parse from {uri} failed: {e}")))
    }
}

/// Extract the `iss` claim from a JWT without validating it. Used to route a
/// token to the right issuer before signature verification; never treat the
/// result as authenticated.
pub fn peek_issuer(token: &str) -> Option<String> {
    let payload_b64 = token.split('.').nth(1)?;
    let unpadded = payload_b64.trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(unpadded)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get("iss")?.as_str().map(String::from)
}

fn find_jwk<'a>(
    jwks: &'a jsonwebtoken::jwk::JwkSet,
    kid: Option<&str>,
) -> Option<&'a jsonwebtoken::jwk::Jwk> {
    match kid {
        Some(kid) => jwks
            .keys
            .iter()
            .find(|k| k.common.key_id.as_deref() == Some(kid)),
        // Single-key JWKS — common with Kubernetes API servers.
        None if jwks.keys.len() == 1 => jwks.keys.first(),
        None => None,
    }
}
