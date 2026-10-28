//! GCP credentials → short-lived OAuth2 access tokens.
//!
//! Vertex AI (Google Agent Platform, fka Vertex) does not accept a standing API
//! key the way the Gemini Developer API does. It authenticates with an OAuth2
//! bearer token that expires in about an hour, so a `vertex` provider
//! connection stores a **service account key** — the JSON blob GCP downloads,
//! holding a PEM-encoded RSA private key — and the router mints tokens from it,
//! refreshing them in the background of whichever request first notices the
//! cached one going stale. The key itself never leaves the process.
//!
//! Credential shapes accepted in the stored credential:
//!   - `service_account` key JSON (RS256 JWT-bearer grant, RFC 7523)
//!   - `authorized_user` JSON (`gcloud auth application-default login`)
//!
//! The stored JSON may also carry an optional `location` alongside the key
//! fields, naming the Vertex region to serve (see [`crate::backend::vertex`]).

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use super::provider::BackendError;

/// Scope requested for Vertex AI calls.
pub const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

/// Assertions are minted with a 1h lifetime, the maximum Google accepts.
const ASSERTION_LIFETIME_SECS: u64 = 3600;

/// Treat a token as expired this many seconds early, so a request that starts
/// just before the deadline never sends a token that lapses mid-flight.
const EXPIRY_SKEW_SECS: u64 = 120;

/// A GCP service account key, as downloaded from the console.
#[derive(Clone, Deserialize)]
pub struct ServiceAccountKey {
    pub client_email: String,
    /// PEM-encoded RSA private key (`-----BEGIN PRIVATE KEY-----`).
    pub private_key: String,
    #[serde(default)]
    pub private_key_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default = "default_token_uri")]
    pub token_uri: String,
    /// Optional operator addition: the Vertex region to serve.
    #[serde(default)]
    pub location: Option<String>,
}

/// ADC user credentials written by `gcloud auth application-default login`.
/// Convenient for local development; a deployment should use a service account.
#[derive(Clone, Deserialize)]
pub struct AuthorizedUserKey {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    /// Billing/quota project. User credentials are rejected by
    /// `aiplatform.googleapis.com` unless one is named, so this doubles as the
    /// project to serve.
    #[serde(default, alias = "project_id")]
    pub quota_project_id: Option<String>,
    #[serde(default = "default_token_uri")]
    pub token_uri: String,
    #[serde(default)]
    pub location: Option<String>,
}

fn default_token_uri() -> String {
    DEFAULT_TOKEN_URI.to_string()
}

/// Redacting `Debug` impls: these structs hold key material, and the point of
/// the feature is that it never leaves the process — including into a log line.
impl std::fmt::Debug for ServiceAccountKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceAccountKey")
            .field("client_email", &self.client_email)
            .field("project_id", &self.project_id)
            .field("private_key", &"<redacted>")
            .finish()
    }
}

impl std::fmt::Debug for AuthorizedUserKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizedUserKey")
            .field("quota_project_id", &self.quota_project_id)
            .field("client_secret", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .finish()
    }
}

/// The credential material a [`GcpTokenSource`] mints tokens from.
#[derive(Clone)]
pub enum GcpCredentials {
    ServiceAccount(Box<ServiceAccountKey>),
    AuthorizedUser(Box<AuthorizedUserKey>),
}

impl std::fmt::Debug for GcpCredentials {
    /// Prints only the credential kind — every variant carries a secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GcpCredentials::{}", self.kind())
    }
}

/// Used to peek at `type` before committing to a concrete shape.
#[derive(Deserialize)]
struct CredentialKind {
    #[serde(default, rename = "type")]
    kind: String,
}

impl GcpCredentials {
    /// Parse a stored credential blob.
    ///
    /// Accepts the credentials JSON as downloaded, or the same JSON
    /// base64-encoded — a key pasted through an env var or a Kubernetes secret
    /// often arrives that way, and the two are unambiguous.
    pub fn parse(raw: &str) -> Result<Self, BackendError> {
        let trimmed = raw.trim();
        let decoded;
        let json = if trimmed.starts_with('{') {
            trimmed
        } else {
            decoded = base64::engine::general_purpose::STANDARD
                .decode(trimmed)
                .map_err(|_| {
                    BackendError::ConfigError(
                        "Vertex credential must be a GCP service account key JSON \
                         (or that JSON base64-encoded)"
                            .to_string(),
                    )
                })
                .and_then(|bytes| {
                    String::from_utf8(bytes).map_err(|_| {
                        BackendError::ConfigError(
                            "base64-decoded Vertex credential is not UTF-8".to_string(),
                        )
                    })
                })?;
            decoded.trim()
        };

        let kind: CredentialKind = serde_json::from_str(json).map_err(|e| {
            BackendError::ConfigError(format!("Vertex credential is not valid JSON: {e}"))
        })?;

        match kind.kind.as_str() {
            "authorized_user" => {
                let key: AuthorizedUserKey = serde_json::from_str(json).map_err(|e| {
                    BackendError::ConfigError(format!(
                        "GCP authorized_user credential is missing fields: {e}"
                    ))
                })?;
                Ok(GcpCredentials::AuthorizedUser(Box::new(key)))
            }
            // Anything else is read as a service account key. The field is
            // normally "service_account", but keys assembled by hand or by a
            // secrets manager sometimes drop it.
            _ => {
                let mut key: ServiceAccountKey = serde_json::from_str(json).map_err(|e| {
                    BackendError::ConfigError(format!(
                        "GCP service account key is missing fields \
                         (client_email and private_key are required): {e}"
                    ))
                })?;
                key.private_key = normalize_pem(&key.private_key);
                Ok(GcpCredentials::ServiceAccount(Box::new(key)))
            }
        }
    }

    /// The GCP project these credentials name, if any.
    pub fn project_id(&self) -> Option<&str> {
        match self {
            GcpCredentials::ServiceAccount(k) => k.project_id.as_deref(),
            GcpCredentials::AuthorizedUser(k) => k.quota_project_id.as_deref(),
        }
    }

    /// The endpoint that receives this credential's signed assertion or refresh
    /// token.
    pub fn token_uri(&self) -> &str {
        match self {
            GcpCredentials::ServiceAccount(k) => &k.token_uri,
            GcpCredentials::AuthorizedUser(k) => &k.token_uri,
        }
    }

    /// The Vertex region the operator wrote into the credential, if any.
    pub fn location(&self) -> Option<&str> {
        match self {
            GcpCredentials::ServiceAccount(k) => k.location.as_deref(),
            GcpCredentials::AuthorizedUser(k) => k.location.as_deref(),
        }
    }

    /// The project to send as `x-goog-user-project`.
    ///
    /// `aiplatform.googleapis.com` rejects *user* credentials outright unless a
    /// quota project is named. Service accounts must NOT send the header: it
    /// would additionally require `serviceusage.services.use` on that project,
    /// turning a working key into a 403.
    pub fn quota_project(&self) -> Option<&str> {
        match self {
            GcpCredentials::AuthorizedUser(k) => k.quota_project_id.as_deref(),
            GcpCredentials::ServiceAccount(_) => None,
        }
    }

    /// Short label for logs and errors.
    pub fn kind(&self) -> &'static str {
        match self {
            GcpCredentials::ServiceAccount(_) => "service_account",
            GcpCredentials::AuthorizedUser(_) => "authorized_user",
        }
    }
}

/// A private key that survived a round-trip through an env var, a YAML literal,
/// or a double-encoded secret can carry `\n` as two characters rather than a
/// newline. `serde_json` already unescapes real JSON, so this only fires on
/// double-escaped input — where the PEM parser would otherwise reject a key
/// that looks perfectly fine to whoever pasted it.
fn normalize_pem(pem: &str) -> String {
    if pem.contains("\\n") {
        pem.replace("\\n", "\n")
    } else {
        pem.to_string()
    }
}

#[derive(Clone)]
struct CachedToken {
    token: String,
    /// Unix seconds at which the token stops being usable.
    expires_at: u64,
}

/// Mints and caches OAuth2 access tokens for one set of GCP credentials.
///
/// Refresh happens under the write lock, so a burst of concurrent requests on a
/// cold or stale cache mints one token between them rather than one each.
pub struct GcpTokenSource {
    credentials: GcpCredentials,
    client: reqwest::Client,
    cached: RwLock<Option<CachedToken>>,
}

impl GcpTokenSource {
    pub fn new(client: reqwest::Client, credentials: GcpCredentials) -> Self {
        Self {
            credentials,
            client,
            cached: RwLock::new(None),
        }
    }

    pub fn credentials(&self) -> &GcpCredentials {
        &self.credentials
    }

    /// A valid access token, minting or refreshing one if the cache cannot serve.
    pub async fn token(&self) -> Result<String, BackendError> {
        if let Some(cached) = self.cached.read().await.as_ref() {
            if cached.expires_at > unix_now() + EXPIRY_SKEW_SECS {
                return Ok(cached.token.clone());
            }
        }

        // Re-check under the write lock: whoever arrives first refreshes, and
        // everyone behind them reads what it stored.
        let mut slot = self.cached.write().await;
        let now = unix_now();
        if let Some(cached) = slot.as_ref() {
            if cached.expires_at > now + EXPIRY_SKEW_SECS {
                return Ok(cached.token.clone());
            }
        }

        let minted = self.mint(now).await?;
        let token = minted.token.clone();
        *slot = Some(minted);
        Ok(token)
    }

    async fn mint(&self, now: u64) -> Result<CachedToken, BackendError> {
        match &self.credentials {
            GcpCredentials::ServiceAccount(key) => {
                let assertion = sign_assertion(key, now)?;
                let form = [
                    ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                    ("assertion", assertion.as_str()),
                ];
                self.post_token_request(&key.token_uri, &form, now).await
            }
            GcpCredentials::AuthorizedUser(key) => {
                let form = [
                    ("grant_type", "refresh_token"),
                    ("client_id", key.client_id.as_str()),
                    ("client_secret", key.client_secret.as_str()),
                    ("refresh_token", key.refresh_token.as_str()),
                ];
                self.post_token_request(&key.token_uri, &form, now).await
            }
        }
    }

    async fn post_token_request(
        &self,
        token_uri: &str,
        form: &[(&str, &str)],
        now: u64,
    ) -> Result<CachedToken, BackendError> {
        let response = self
            .client
            .post(token_uri)
            .form(form)
            .send()
            .await
            .map_err(|e| BackendError::ConfigError(format!("GCP token request failed: {e}")))?;

        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
            #[serde(default)]
            expires_in: Option<u64>,
        }

        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable body>".to_string());

        if !status.is_success() {
            return Err(BackendError::api_status(
                status.as_u16(),
                format!("GCP token endpoint rejected the service account key: {body}"),
            ));
        }

        let parsed: TokenResponse = serde_json::from_str(&body)
            .map_err(|e| BackendError::Other(format!("failed to parse GCP token response: {e}")))?;

        Ok(CachedToken {
            token: parsed.access_token,
            expires_at: now + parsed.expires_in.unwrap_or(ASSERTION_LIFETIME_SECS),
        })
    }
}

/// Sign the RFC 7523 JWT-bearer assertion with the key's PEM. The audience is
/// the credential's own `token_uri` — the endpoint that will verify it.
fn sign_assertion(key: &ServiceAccountKey, now: u64) -> Result<String, BackendError> {
    #[derive(Serialize)]
    struct Claims<'a> {
        iss: &'a str,
        scope: &'a str,
        aud: &'a str,
        exp: u64,
        iat: u64,
    }

    let mut header = Header::new(Algorithm::RS256);
    header.kid = key.private_key_id.clone();

    let encoding_key = EncodingKey::from_rsa_pem(key.private_key.as_bytes()).map_err(|e| {
        BackendError::ConfigError(format!(
            "GCP service account private_key is not a usable RSA PEM: {e}"
        ))
    })?;

    let claims = Claims {
        iss: &key.client_email,
        scope: CLOUD_PLATFORM_SCOPE,
        aud: &key.token_uri,
        exp: now + ASSERTION_LIFETIME_SECS,
        iat: now,
    };

    jsonwebtoken::encode(&header, &claims, &encoding_key)
        .map_err(|e| BackendError::ConfigError(format!("failed to sign GCP JWT assertion: {e}")))
}

impl std::fmt::Debug for GcpTokenSource {
    /// Hand-written so a stray `{:?}` can never print key material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcpTokenSource")
            .field("credentials", &self.credentials.kind())
            .finish()
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SA_JSON: &str = r#"{
        "type": "service_account",
        "project_id": "my-proj",
        "private_key_id": "abc123",
        "private_key": "-----BEGIN PRIVATE KEY-----\nMIIB\n-----END PRIVATE KEY-----\n",
        "client_email": "router@my-proj.iam.gserviceaccount.com",
        "token_uri": "https://oauth2.googleapis.com/token"
    }"#;

    #[test]
    fn parses_service_account_key() {
        let creds = GcpCredentials::parse(SA_JSON).expect("parse");
        assert_eq!(creds.kind(), "service_account");
        assert_eq!(creds.project_id(), Some("my-proj"));
        // Only user credentials may send a quota project.
        assert_eq!(creds.quota_project(), None);
        match creds {
            GcpCredentials::ServiceAccount(key) => {
                assert_eq!(key.client_email, "router@my-proj.iam.gserviceaccount.com");
                assert!(key.private_key.starts_with("-----BEGIN PRIVATE KEY-----"));
                assert!(!key.private_key.contains("\\n"));
            }
            other => panic!("expected service account, got {other:?}"),
        }
    }

    #[test]
    fn parses_base64_encoded_key() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(SA_JSON);
        assert_eq!(
            GcpCredentials::parse(&encoded).expect("parse").project_id(),
            Some("my-proj")
        );
    }

    #[test]
    fn repairs_double_escaped_pem_newlines() {
        let mangled = SA_JSON.replace("\\n", "\\\\n");
        match GcpCredentials::parse(&mangled).expect("parse") {
            GcpCredentials::ServiceAccount(key) => {
                assert!(!key.private_key.contains("\\n"));
                assert!(key.private_key.contains('\n'));
            }
            other => panic!("expected service account, got {other:?}"),
        }
    }

    #[test]
    fn reads_an_operator_supplied_location() {
        let raw = SA_JSON.replace(
            r#""type": "service_account","#,
            r#""type": "service_account", "location": "europe-west4","#,
        );
        assert_eq!(
            GcpCredentials::parse(&raw).expect("parse").location(),
            Some("europe-west4")
        );
    }

    #[test]
    fn parses_authorized_user_and_sends_a_quota_project() {
        let raw = r#"{
            "type": "authorized_user",
            "client_id": "cid",
            "client_secret": "csecret",
            "refresh_token": "rtoken",
            "quota_project_id": "my-proj"
        }"#;
        let creds = GcpCredentials::parse(raw).expect("parse");
        assert_eq!(creds.kind(), "authorized_user");
        assert_eq!(creds.project_id(), Some("my-proj"));
        // aiplatform.googleapis.com refuses user credentials without this.
        assert_eq!(creds.quota_project(), Some("my-proj"));
    }

    #[test]
    fn rejects_a_plain_api_key_with_an_actionable_message() {
        let err = GcpCredentials::parse("AIzaSyNotAServiceAccountKey").expect_err("must fail");
        let message = err.to_string();
        assert!(
            message.contains("service account key"),
            "unhelpful: {message}"
        );
    }

    #[test]
    fn missing_key_fields_are_a_config_error() {
        let err = GcpCredentials::parse(r#"{"type":"service_account"}"#).expect_err("must fail");
        assert!(err.to_string().contains("client_email"), "got {err}");
    }

    #[test]
    fn debug_never_prints_key_material() {
        let source = GcpTokenSource::new(
            reqwest::Client::new(),
            GcpCredentials::parse(SA_JSON).expect("parse"),
        );
        let rendered = format!("{source:?}");
        assert!(!rendered.contains("PRIVATE KEY"), "leaked: {rendered}");
        assert!(rendered.contains("service_account"));
    }
}
