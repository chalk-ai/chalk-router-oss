use crate::{
    api_keys::ApiKey,
    cluster_auth::ChalkClaims,
    types::{Error, ErrorResponse},
    AppState,
};
use axum::{
    body::Body,
    extract::Request,
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use dashmap::DashMap;
use sha2::{Digest, Sha256};
use std::sync::{Arc, OnceLock};

/// Claims read from an already-verified JWT payload. The federated-auth crate
/// normalizes tokens down to sub/exp/scopes and drops the rest, so the payload
/// is decoded again here, once per verification. `environment_id` stays lenient:
/// a token without it is allowed through (the guard is opt-in, matching
/// shared-gateway tokens that carry no environment). `chalk` is `None` when the
/// payload does not have the supported Chalk claim shape; cluster and admin
/// authentication reject such tokens. Inference retains its legacy scope guard.
struct PayloadClaims {
    audience_matches: bool,
    environment_id: Option<String>,
    chalk: Option<Arc<ChalkClaims>>,
}

fn payload_claims(token: &str, expected_audience: Option<&str>) -> PayloadClaims {
    use base64::Engine as _;
    let payload = token
        .split('.')
        .nth(1)
        .and_then(|payload| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(payload)
                .ok()
        })
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let Some(payload) = payload else {
        return PayloadClaims {
            audience_matches: false,
            environment_id: None,
            chalk: None,
        };
    };
    let environment_id = payload
        .get("environment_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let audience_matches = expected_audience.is_none_or(|expected| match payload.get("aud") {
        Some(serde_json::Value::String(value)) => value == expected,
        Some(serde_json::Value::Array(values)) => {
            !values.is_empty()
                && values.iter().all(|v| v.is_string())
                && values.iter().any(|v| v.as_str() == Some(expected))
        }
        _ => false,
    });
    PayloadClaims {
        audience_matches,
        environment_id,
        // Compact permission claims require a dedicated decoder (META-847).
        // Never interpret an unsupported restricted grant as an absent legacy
        // permission list, which some token kinds treat as unrestricted.
        chalk: if payload.get("p").is_some() {
            None
        } else {
            serde_json::from_value(payload).ok().map(Arc::new)
        },
    }
}

// ---------------------------------------------------------------------------
// Verified-token cache
// ---------------------------------------------------------------------------
//
// Federated Chalk JWT auth pays an RS256/ES256 signature verification
// (`chalk_federated_auth::OidcRegistry::try_validate`) on every request, plus
// a second base64+JSON parse of the same token in `payload_claims` (the
// federated-auth crate's `Claims` carries neither `environment_id` nor the
// Chalk claims cluster authentication needs). Both
// are skippable on repeat requests from the same caller (e.g. a long-lived
// gRPC client) by caching the verification result.
//
// Keyed by the SHA-256 bytes of the raw bearer token — never the token itself,
// which must never be stored or logged. The fixed-size digest avoids allocating
// a hex `String` for every request. A `static OnceLock<DashMap<..>>`
// rather than an `AppState` field: the cache is process-local (no
// invalidation story needed, entries just expire) and every call site here
// already has the raw token in hand.

/// A federated JWT verification worth remembering: the normalized claims plus
/// the token's own `environment_id` and Chalk claims (parsed once, on the miss
/// path, and reused for both the immediate checks and this cache entry).
struct CachedVerification {
    expected_audience: Option<String>,
    registry: Arc<chalk_federated_auth::OidcRegistry>,
    claims: chalk_federated_auth::Claims,
    environment_id: Option<String>,
    chalk_claims: Option<Arc<ChalkClaims>>,
    /// Unix-seconds deadline after which this entry must be treated as a miss
    /// even if still present: `min(token exp, insert time + 300s)`. The cap
    /// keeps a long-lived token from pinning a verification result for its
    /// entire lifetime — the cache is a latency optimization, not a substitute
    /// for periodic re-verification.
    expires_at: u64,
}

/// TTL cap applied on top of the token's own `exp`, in seconds.
const JWT_CACHE_TTL_CAP_SECS: u64 = 300;
/// Hard bound on cache entries. An insert that pushes the map past this first
/// sweeps expired entries, then — if the map is still over the low watermark —
/// evicts the soonest-to-expire entries down to it, so the cache stays bounded
/// even under a stream of distinct, still-valid tokens.
const JWT_CACHE_MAX_ENTRIES: usize = 4096;
/// Eviction target after a sweep. Evicting down to a watermark below the max
/// (rather than to the max itself) means the O(n) sweep amortizes over the
/// next ~(MAX - WATERMARK) inserts instead of running on every one.
const JWT_CACHE_LOW_WATERMARK: usize = 3072;

type TokenHash = [u8; 32];

fn jwt_verification_cache() -> &'static DashMap<TokenHash, CachedVerification> {
    static CACHE: OnceLock<DashMap<TokenHash, CachedVerification>> = OnceLock::new();
    CACHE.get_or_init(DashMap::new)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// SHA-256 digest of the raw token — the cache key. Never log or store the
/// token itself; this digest is one-way and safe to hold.
fn hash_token(token: &str) -> TokenHash {
    Sha256::digest(token.as_bytes()).into()
}

/// The outcome of validating a federated Chalk JWT: the normalized claims, the
/// token's `environment_id` claim, and its Chalk claims (payload parsed at most
/// once per verification, never on a cache hit).
pub(crate) struct FederatedAuthResult {
    pub(crate) claims: chalk_federated_auth::Claims,
    environment_id: Option<String>,
    pub(crate) chalk_claims: Option<Arc<ChalkClaims>>,
}

/// Validate a federated Chalk JWT, consulting the verified-token cache first.
///
/// Returns `None` when the token's `iss` doesn't match any configured issuer
/// (the caller falls back to its own auth scheme, exactly as
/// `OidcRegistry::try_validate` behaves). Returns `Some(Err)` for a token
/// that matched an issuer but failed verification. Only successful
/// verifications are cached; a cache hit skips both the signature
/// verification and the payload re-parse.
pub(crate) async fn validate_federated_token(
    state: &AppState,
    token: &str,
) -> Option<Result<FederatedAuthResult, chalk_federated_auth::Error>> {
    let key = hash_token(token);

    if let Some(entry) = jwt_verification_cache().get(&key) {
        if now_unix() < entry.expires_at
            && Arc::ptr_eq(&entry.registry, &state.federated_auth)
            && entry.expected_audience == state.federated_jwt_audience
        {
            return Some(Ok(FederatedAuthResult {
                claims: entry.claims.clone(),
                environment_id: entry.environment_id.clone(),
                chalk_claims: entry.chalk_claims.clone(),
            }));
        }
        // Expired (either the real `exp` or our TTL cap) — drop it now rather
        // than waiting for the next sweep, and fall through to a full
        // re-verification below.
        drop(entry);
        jwt_verification_cache().remove(&key);
    }

    let result = state.federated_auth.try_validate(token).await?;
    let claims = match result {
        Ok(claims) => claims,
        Err(e) => return Some(Err(e)),
    };

    // Parsed exactly once here — reused for the caller's checks AND the cache
    // entry below, never re-parsed on a hit.
    let PayloadClaims {
        audience_matches,
        environment_id,
        chalk: chalk_claims,
    } = payload_claims(token, state.federated_jwt_audience.as_deref());
    // The shared verifier checks a supplied aud but does not require its presence.
    // Require the configured audience before any successful result can be cached.
    if !audience_matches {
        return Some(Err(chalk_federated_auth::Error::Auth(
            "invalid audience".into(),
        )));
    }

    // Cache only successful verifications. `exp == 0` means the issuer
    // doesn't require `exp` (see `IssuerConfig::require_exp`); such tokens
    // carry no real deadline to cap against, so skip caching them rather than
    // inventing one.
    if claims.exp > 0 {
        let expires_at = (claims.exp as u64).min(now_unix().saturating_add(JWT_CACHE_TTL_CAP_SECS));
        let cache = jwt_verification_cache();
        cache.insert(
            key,
            CachedVerification {
                expected_audience: state.federated_jwt_audience.clone(),
                registry: state.federated_auth.clone(),
                claims: claims.clone(),
                environment_id: environment_id.clone(),
                chalk_claims: chalk_claims.clone(),
                expires_at,
            },
        );
        if cache.len() > JWT_CACHE_MAX_ENTRIES {
            let now = now_unix();
            cache.retain(|_, v| v.expires_at > now);
            // Expired-entry sweeping alone doesn't bound the map — a stream of
            // distinct still-valid tokens would grow it forever. Evict the
            // soonest-to-expire entries down to the watermark; they're the
            // closest to dying anyway, and an evicted-but-valid token just
            // re-verifies on its next request.
            let excess = cache.len().saturating_sub(JWT_CACHE_LOW_WATERMARK);
            if excess > 0 {
                let mut by_expiry: Vec<(TokenHash, u64)> = cache
                    .iter()
                    .map(|e| (*e.key(), e.value().expires_at))
                    .collect();
                by_expiry.sort_unstable_by_key(|(_, expires_at)| *expires_at);
                for (key, _) in by_expiry.into_iter().take(excess) {
                    cache.remove(&key);
                }
            }
        }
    }

    Some(Ok(FederatedAuthResult {
        claims,
        environment_id,
        chalk_claims,
    }))
}

pub(crate) fn unauthorized(message: impl Into<String>) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse {
            error: Error {
                message: message.into(),
                error_type: "authentication_error".to_string(),
                param: None,
                code: Some("invalid_api_key".to_string()),
            },
        }),
    )
        .into_response()
}

pub(crate) fn forbidden(message: impl Into<String>) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(ErrorResponse {
            error: Error {
                message: message.into(),
                error_type: "permission_error".to_string(),
                param: None,
                code: Some("access_denied".to_string()),
            },
        }),
    )
        .into_response()
}

pub(crate) fn is_skip_path(path: &str) -> bool {
    path == "/health"
        || path == "/v1/healthcheck"
        // Standard gRPC health probes cannot attach application credentials.
        || path.starts_with("/grpc.health.v1.Health/")
}

/// True for paths that should require admin authentication. The `/v1/admin/`
/// and `/v1/debug/` prefixes stay gated as defence in depth even though the
/// HTTP JSON admin routes were removed (the gRPC services under
/// `/chalk.router.v1.` are the only admin surface now); an unauthenticated
/// probe against either prefix must still see 401/403, not a 404 that would
/// reveal the route is gone.
pub(crate) fn is_admin_path(path: &str) -> bool {
    path.starts_with("/v1/admin/")
        || path.starts_with("/v1/debug/")
        || path.starts_with("/chalk.router.v1.")
}

/// Whether a federated JWT may be used on this process.
///
/// Tokens with no `environment_id` claim are allowed (the guard is opt-in,
/// matching user JWTs and shared-gateway cookies). Tokens that name this
/// process's `CHALK_ENVIRONMENT_ID` are allowed. The shared go-api-server
/// proxy forwards a JWT that still names the caller's environment; set
/// `ROUTER_ACCEPT_CROSS_ENVIRONMENT_TOKENS` on that instance so those hops
/// are admitted. Per-environment routers leave the flag off so a token from
/// env B cannot be used against env A's instance.
fn federated_token_environment_ok(state: &AppState, token_env: Option<&str>) -> bool {
    match token_env {
        None => true,
        Some(tenv) if tenv == state.environment_id => true,
        Some(_) => state.accept_cross_environment_tokens,
    }
}

pub(crate) fn extract_bearer(req: &Request) -> Option<String> {
    if let Some(auth) = req.headers().get(header::AUTHORIZATION) {
        if let Ok(auth_str) = auth.to_str() {
            if let Some(token) = auth_str.strip_prefix("Bearer ") {
                return Some(token.trim().to_string());
            }
        }
    }
    if let Some(key_header) = req.headers().get("x-api-key") {
        if let Ok(token) = key_header.to_str() {
            return Some(token.trim().to_string());
        }
    }
    // Gemini SDKs use this header. It is a router key, never a provider credential.
    if req.uri().path() == "/v1beta/interactions"
        || req.uri().path().starts_with("/v1beta/interactions/")
    {
        if let Some(token) = req
            .headers()
            .get("x-goog-api-key")
            .and_then(|value| value.to_str().ok())
        {
            return Some(token.trim().to_string());
        }
    }
    None
}

/// Router API keys are opaque credentials for the proxied surface. Recognize
/// their stable prefix here only to give an actionable control-plane error; do
/// not look them up or reveal whether a particular key exists.
fn is_router_api_key(token: &str) -> bool {
    token.starts_with("ck-")
}

pub async fn auth_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();

    if is_skip_path(&path) {
        return next.run(req).await;
    }

    if is_admin_path(&path) {
        let admin_key = state.admin_key.as_deref();
        if admin_key.is_none() && state.federated_auth.is_empty() {
            tracing::warn!(
                "Admin request rejected: no admin auth configured (ROUTER_ADMIN_KEY or CHALK_JWT_ISSUERS)"
            );
            return forbidden("Admin key not configured");
        }
        let token = match extract_bearer(&req) {
            Some(t) => t,
            None => {
                tracing::warn!("Admin request rejected: missing token");
                return unauthorized("Missing admin key");
            }
        };
        if admin_key == Some(token.as_str()) {
            return next.run(req).await;
        }

        if is_router_api_key(&token) {
            tracing::warn!(path = %path, "Admin request rejected: router API key presented");
            return forbidden(
                "Router API keys cannot access control-plane RPCs; use a Chalk client JWT.",
            );
        }

        // Federated Chalk JWT: a token whose `iss` matches a configured Chalk
        // API server is validated against that server's JWKS. Non-JWT tokens
        // (e.g. a mistyped admin key) have no matching issuer, return None
        // here, and fall through to the rejection below. Verification is
        // cached (see `validate_federated_token`), so a repeat request on a
        // long-lived connection skips both the signature check and the
        // environment_id re-parse.
        if let Some(result) = validate_federated_token(&state, &token).await {
            match result {
                Ok(FederatedAuthResult {
                    claims,
                    chalk_claims,
                    ..
                }) => {
                    let Some(chalk_claims) = chalk_claims else {
                        return unauthorized("invalid Chalk claims");
                    };
                    if req.headers().get("x-chalk-env-id").is_some_and(|value| {
                        value.to_str().ok() != Some(state.environment_id.as_str())
                    }) {
                        return forbidden(
                            "x-chalk-env-id conflicts with the configured environment",
                        );
                    }
                    if let Err(error) = chalk_claims.authorize_environment_admin(
                        &state.environment_id,
                        state.federated_admin_team_id.as_deref(),
                        &path,
                    ) {
                        return forbidden(error);
                    }
                    tracing::debug!(
                        path = %path,
                        user = %claims.sub,
                        "Admin request authenticated via federated Chalk JWT"
                    );
                    req.extensions_mut().insert(Arc::new(claims));
                    req.extensions_mut().insert(chalk_claims);
                    return next.run(req).await;
                }
                Err(e) => {
                    tracing::warn!(path = %path, "Admin request rejected: federated JWT invalid: {e}");
                    return unauthorized("Invalid federated token");
                }
            }
        }

        tracing::warn!("Admin request rejected: invalid token");
        return unauthorized("Authentication failed");
    }

    let token = extract_bearer(&req);
    let (api_key, authenticated_principal) = if let Some(ref t) = token {
        // Allow admin key to access non-admin endpoints (used by the UI)
        if state.admin_key.as_deref() == Some(t.as_str()) {
            (
                Arc::new(crate::api_keys::ApiKey {
                    key_hash: "__admin__".to_string(),
                    key_prefix: "__admin__".to_string(),
                    description: "Admin key".to_string(),
                    created_at: chrono::Utc::now(),
                    permission: crate::api_keys::ApiKeyPermission::default(),
                    labels: std::collections::HashMap::new(),
                    usage_pool_id: None,
                    daily_token_budget: None,
                }),
                None,
            )
        } else {
            // Federated JWTs create a synthetic credential identity. Plain API
            // keys have no matching issuer and fall through to the key store.
            match validate_federated_token(&state, t).await {
                Some(Ok(FederatedAuthResult {
                    claims,
                    environment_id,
                    ..
                })) => {
                    if !federated_token_environment_ok(&state, environment_id.as_deref()) {
                        tracing::warn!(path = %path, token_env = %environment_id.as_deref().unwrap_or(""), expected = %state.environment_id, "Request rejected: token environment mismatch");
                        return forbidden("token is scoped to a different environment");
                    }
                    let principal_id = Arc::<str>::from(claims.sub.as_str());
                    let authenticated_principal = AuthenticatedPrincipal {
                        id: principal_id.clone(),
                        principal_type: "service_token",
                        user_id: Some(principal_id),
                    };
                    let synthetic = Arc::new(crate::api_keys::ApiKey {
                        key_hash: format!("federated:{}", claims.sub),
                        key_prefix: "federated".to_string(),
                        description: "Federated Chalk JWT".to_string(),
                        created_at: chrono::Utc::now(),
                        permission: crate::api_keys::ApiKeyPermission::default(),
                        labels: std::collections::HashMap::new(),
                        usage_pool_id: None,
                        daily_token_budget: None,
                    });
                    tracing::debug!(
                        subject_present = !claims.sub.is_empty(),
                        "Request authenticated via federated Chalk JWT"
                    );
                    (synthetic, Some(authenticated_principal))
                }
                Some(Err(e)) => {
                    tracing::warn!(path = %path, "Request rejected: federated JWT invalid: {e}");
                    return unauthorized("Invalid federated token");
                }
                None => {
                    let api_key = match state.api_key_store.authenticate_checked(t).await {
                        Ok(Some(key)) => key,
                        Ok(None) => {
                            tracing::warn!(path = %path, "Request rejected: invalid API key");
                            return unauthorized("Invalid API key");
                        }
                        Err(error) => {
                            tracing::error!(path = %path, %error, "API-key authentication storage unavailable");
                            return ErrorResponse::new(
                                StatusCode::SERVICE_UNAVAILABLE,
                                "server_error",
                                "Authentication temporarily unavailable",
                            )
                            .into_response();
                        }
                    };
                    let authenticated_principal = AuthenticatedPrincipal {
                        id: Arc::from(api_key.key_hash.as_str()),
                        principal_type: "router_api_key",
                        user_id: None,
                    };
                    (api_key, Some(authenticated_principal))
                }
            }
        }
    } else if state.dangerously_permit_unauthenticated
        && !req.headers().contains_key(header::AUTHORIZATION)
        && !req.headers().contains_key("x-api-key")
        && !req.headers().contains_key("x-goog-api-key")
    {
        (
            Arc::new(crate::api_keys::ApiKey {
                key_hash: "__anonymous__".to_string(),
                key_prefix: "anonymous".to_string(),
                description: "Anonymous local development".to_string(),
                created_at: chrono::Utc::now(),
                permission: crate::api_keys::ApiKeyPermission::default(),
                labels: std::collections::HashMap::new(),
                usage_pool_id: None,
                daily_token_budget: None,
            }),
            None,
        )
    } else {
        tracing::warn!(path = %path, "Request rejected: missing API key");
        return unauthorized(
            "Missing API key. Provide Authorization: Bearer <key> or X-Api-Key header",
        );
    };

    req.extensions_mut().insert(api_key);
    if let Some(authenticated_principal) = authenticated_principal {
        req.extensions_mut().insert(authenticated_principal);
    }
    next.run(req).await
}

#[derive(Clone)]
pub(crate) struct AuthenticatedPrincipal {
    id: Arc<str>,
    principal_type: &'static str,
    user_id: Option<Arc<str>>,
}

#[derive(Clone)]
pub struct ResolvedApiKey(pub Arc<ApiKey>, Option<AuthenticatedPrincipal>);

impl ResolvedApiKey {
    /// The server-verified caller identity: a federated JWT's `sub`, or an
    /// opaque HMAC-backed identity for a stored router API key. Static admin
    /// credentials and anonymous requests do not claim a principal.
    pub fn authenticated_principal_id(&self) -> Option<&str> {
        self.1.as_ref().map(|principal| principal.id.as_ref())
    }

    pub fn authenticated_principal_type(&self) -> Option<&str> {
        self.1.as_ref().map(|principal| principal.principal_type)
    }

    pub fn authenticated_user_id(&self) -> Option<&str> {
        self.1
            .as_ref()
            .and_then(|principal| principal.user_id.as_deref())
    }
}

impl<S> axum::extract::FromRequestParts<S> for ResolvedApiKey
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, Json<ErrorResponse>);

    fn from_request_parts<'life0, 'life1, 'async_trait>(
        parts: &'life0 mut axum::http::request::Parts,
        _state: &'life1 S,
    ) -> ::core::pin::Pin<
        Box<
            dyn ::core::future::Future<Output = Result<Self, Self::Rejection>>
                + ::core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let api_key = parts
                .extensions
                .get::<Arc<ApiKey>>()
                .cloned()
                .ok_or_else(|| {
                    (
                        StatusCode::UNAUTHORIZED,
                        Json(ErrorResponse {
                            error: Error {
                                message: "Missing API key".to_string(),
                                error_type: "authentication_error".to_string(),
                                param: None,
                                code: Some("invalid_api_key".to_string()),
                            },
                        }),
                    )
                })?;
            let authenticated_principal = parts.extensions.get::<AuthenticatedPrincipal>().cloned();
            Ok(ResolvedApiKey(api_key, authenticated_principal))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_hash_is_fixed_size_and_stable() {
        let expected: TokenHash = Sha256::digest(b"a.b.c").into();
        assert_eq!(hash_token("a.b.c"), expected);
        assert_ne!(hash_token("a.b.c"), hash_token("a.b.d"));
        assert_eq!(std::mem::size_of::<TokenHash>(), 32);
    }
}

impl AuthenticatedPrincipal {
    pub(crate) fn federated(subject: &str) -> Self {
        let id = Arc::<str>::from(subject);
        Self {
            id: id.clone(),
            principal_type: "service_token",
            user_id: Some(id),
        }
    }
    pub(crate) fn api_key(hash: &str) -> Self {
        Self {
            id: Arc::from(hash),
            principal_type: "router_api_key",
            user_id: None,
        }
    }
}
