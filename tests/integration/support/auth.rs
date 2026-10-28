//! Shared JWT/OIDC-registry helpers for the federated-auth admin and proxy
//! tests. `federated_admin_test` and `federated_proxy_test` each configure
//! their own HS256 `secret_env` issuer (a distinct issuer string, secret, and
//! env var name per file, so the two suites' process-global env vars can't
//! clobber each other) but build and sign tokens the same way.

use std::collections::HashMap;
use std::sync::Arc;

use chalk_federated_auth::{IssuerConfig, OidcRegistry};
use jsonwebtoken::{encode, EncodingKey, Header};

/// Build a single-issuer HS256 registry, mirroring the production shape: an
/// issuer validated against an explicit key source (an env var) rather than
/// OIDC discovery, so no JWKS server is needed in tests. Sets `secret_env` in
/// the process environment to `secret` as a side effect.
pub fn federated_registry(issuer: &str, secret_env: &str, secret: &str) -> Arc<OidcRegistry> {
    federated_registry_with_audience(issuer, secret_env, secret, None)
}

pub fn federated_registry_with_audience(
    issuer: &str,
    secret_env: &str,
    secret: &str,
    audience: Option<&str>,
) -> Arc<OidcRegistry> {
    std::env::set_var(secret_env, secret);
    let config = IssuerConfig {
        issuer: issuer.to_string(),
        jwks_uri: None,
        discovery_url: None,
        secret_env: Some(secret_env.to_string()),
        audience: audience.map(str::to_owned),
        claims_map: HashMap::new(),
        require_exp: true,
    };
    Arc::new(OidcRegistry::from_config(&[config]).expect("registry"))
}

/// Sign `claims` with `secret` using HS256, mirroring how a Chalk
/// metadata-server-issued user JWT is signed.
pub fn chalk_jwt(secret: &str, claims: &serde_json::Value) -> String {
    encode(
        &Header::default(),
        claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .expect("encode token")
}

/// A Unix timestamp 10 minutes in the future, for an `exp` claim that won't
/// trip on a slow CI run.
pub fn future_exp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_secs()
        + 600
}

/// Shape of a Chalk metadata-server user JWT (`kind`/`team_id` are carried
/// along but only `iss`/`sub`/`exp` matter to the auth middleware).
pub fn user_claims(issuer: &str, sub: &str) -> serde_json::Value {
    serde_json::json!({
        "iss": issuer,
        "sub": sub,
        "kind": "user",
        "team_id": "team_456",
        "exp": future_exp(),
    })
}
