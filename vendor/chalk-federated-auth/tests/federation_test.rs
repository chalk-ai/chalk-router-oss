use std::collections::HashMap;

use base64::Engine;
use chalk_federated_auth::{peek_issuer, IssuerConfig, OidcRegistry};
use jsonwebtoken::{encode, EncodingKey, Header};
use rsa::traits::PublicKeyParts;

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn hs256_token(secret: &str, claims: &serde_json::Value) -> String {
    encode(
        &Header::default(),
        claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .expect("encode HS256 token")
}

fn future_exp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_secs()
        + 600
}

fn secret_issuer(issuer: &str, env_name: &str, secret: &str) -> IssuerConfig {
    // SAFETY-of-tests note: each test uses a unique env var name, so parallel
    // test threads never race on the same variable.
    std::env::set_var(env_name, secret);
    IssuerConfig {
        issuer: issuer.to_string(),
        jwks_uri: None,
        discovery_url: None,
        secret_env: Some(env_name.to_string()),
        audience: None,
        claims_map: HashMap::new(),
        require_exp: true,
    }
}

#[tokio::test]
async fn hs256_roundtrip_and_claim_mapping() {
    let mut config = secret_issuer("chalk-test-hs256", "FED_TEST_SECRET_A", "s3cret");
    config.claims_map = HashMap::from([
        ("sub".to_string(), "email".to_string()),
        ("scopes".to_string(), "permissions".to_string()),
    ]);
    let registry = OidcRegistry::from_config(&[config]).expect("registry");

    let token = hs256_token(
        "s3cret",
        &serde_json::json!({
            "iss": "chalk-test-hs256",
            "email": "andy@chalk.ai",
            "permissions": ["team:admin"],
            "exp": future_exp(),
        }),
    );

    let claims = registry
        .try_validate(&token)
        .await
        .expect("issuer should match")
        .expect("token should validate");
    assert_eq!(claims.sub, "andy@chalk.ai");
    assert_eq!(claims.scopes, vec!["team:admin".to_string()]);
}

#[tokio::test]
async fn unknown_issuer_returns_none() {
    let config = secret_issuer("chalk-test-known", "FED_TEST_SECRET_B", "s3cret");
    let registry = OidcRegistry::from_config(&[config]).expect("registry");

    let token = hs256_token(
        "s3cret",
        &serde_json::json!({ "iss": "someone-else", "sub": "u", "exp": future_exp() }),
    );
    assert!(registry.try_validate(&token).await.is_none());

    // Tokens with no iss at all also fall through to the caller.
    let token = hs256_token("s3cret", &serde_json::json!({ "sub": "u" }));
    assert!(registry.try_validate(&token).await.is_none());
}

#[tokio::test]
async fn empty_registry_matches_nothing() {
    let registry = OidcRegistry::empty();
    assert!(registry.is_empty());
    let token = hs256_token(
        "whatever",
        &serde_json::json!({ "iss": "x", "sub": "u", "exp": future_exp() }),
    );
    assert!(registry.try_validate(&token).await.is_none());
}

#[tokio::test]
async fn wrong_secret_rejected() {
    let config = secret_issuer("chalk-test-wrong-secret", "FED_TEST_SECRET_C", "right");
    let registry = OidcRegistry::from_config(&[config]).expect("registry");

    let token = hs256_token(
        "wrong",
        &serde_json::json!({ "iss": "chalk-test-wrong-secret", "sub": "u", "exp": future_exp() }),
    );
    let result = registry
        .try_validate(&token)
        .await
        .expect("issuer should match");
    assert!(result.is_err());
}

#[tokio::test]
async fn expired_token_rejected() {
    let config = secret_issuer("chalk-test-expired", "FED_TEST_SECRET_D", "s3cret");
    let registry = OidcRegistry::from_config(&[config]).expect("registry");

    let token = hs256_token(
        "s3cret",
        &serde_json::json!({ "iss": "chalk-test-expired", "sub": "u", "exp": 1 }),
    );
    let result = registry
        .try_validate(&token)
        .await
        .expect("issuer should match");
    assert!(result.is_err());
}

#[tokio::test]
async fn require_exp_false_allows_missing_exp() {
    let mut config = secret_issuer("chalk-test-noexp", "FED_TEST_SECRET_E", "s3cret");
    config.require_exp = false;
    let registry = OidcRegistry::from_config(&[config]).expect("registry");

    let token = hs256_token(
        "s3cret",
        &serde_json::json!({ "iss": "chalk-test-noexp", "sub": "u" }),
    );
    let claims = registry
        .try_validate(&token)
        .await
        .expect("issuer should match")
        .expect("token without exp should validate when require_exp = false");
    assert_eq!(claims.sub, "u");
    assert_eq!(claims.exp, 0);
}

#[tokio::test]
async fn audience_validation() {
    let mut config = secret_issuer("chalk-test-aud", "FED_TEST_SECRET_F", "s3cret");
    config.audience = Some("chalk-router".to_string());
    let registry = OidcRegistry::from_config(&[config]).expect("registry");

    let good = hs256_token(
        "s3cret",
        &serde_json::json!({
            "iss": "chalk-test-aud", "sub": "u", "aud": "chalk-router", "exp": future_exp(),
        }),
    );
    assert!(registry.try_validate(&good).await.expect("match").is_ok());

    let bad = hs256_token(
        "s3cret",
        &serde_json::json!({
            "iss": "chalk-test-aud", "sub": "u", "aud": "other", "exp": future_exp(),
        }),
    );
    assert!(registry.try_validate(&bad).await.expect("match").is_err());
}

#[tokio::test]
async fn missing_secret_env_is_config_error() {
    let config = IssuerConfig {
        issuer: "chalk-test-missing-env".to_string(),
        jwks_uri: None,
        discovery_url: None,
        secret_env: Some("FED_TEST_SECRET_DOES_NOT_EXIST".to_string()),
        audience: None,
        claims_map: HashMap::new(),
        require_exp: true,
    };
    assert!(OidcRegistry::from_config(&[config]).is_err());
}

#[tokio::test]
async fn non_url_issuer_without_keys_is_config_error() {
    let config = IssuerConfig {
        issuer: "not-a-url".to_string(),
        jwks_uri: None,
        discovery_url: None,
        secret_env: None,
        audience: None,
        claims_map: HashMap::new(),
        require_exp: true,
    };
    assert!(OidcRegistry::from_config(&[config]).is_err());
}

#[test]
fn peek_issuer_reads_unverified_iss() {
    let token = hs256_token(
        "any",
        &serde_json::json!({ "iss": "https://api.chalk.ai", "sub": "u" }),
    );
    assert_eq!(peek_issuer(&token).as_deref(), Some("https://api.chalk.ai"));
    assert_eq!(peek_issuer("not-a-jwt"), None);
}

/// Spin up a local OIDC issuer (discovery + JWKS over HTTP) and validate an
/// RS256 token end-to-end — the exact shape of federation with Chalk's
/// metadata API server, which signs RS256 with a `kid` header and serves
/// `/.well-known/openid-configuration` + `/.well-known/jwks.json`.
#[tokio::test]
async fn rs256_via_oidc_discovery() {
    let mut rng = rand::thread_rng();
    let private_key = rsa::RsaPrivateKey::new(&mut rng, 2048).expect("generate RSA key");
    let public_key = private_key.to_public_key();

    let pem =
        rsa::pkcs1::EncodeRsaPrivateKey::to_pkcs1_pem(&private_key, rsa::pkcs8::LineEnding::LF)
            .expect("encode private key PEM");
    let encoding_key = EncodingKey::from_rsa_pem(pem.as_bytes()).expect("private key");

    let jwk = serde_json::json!({
        "kty": "RSA",
        "use": "sig",
        "alg": "RS256",
        "kid": "test-key-1",
        "n": b64url(&public_key.n().to_bytes_be()),
        "e": b64url(&public_key.e().to_bytes_be()),
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let issuer = format!("http://{addr}");

    let jwks_uri = format!("{issuer}/.well-known/jwks.json");
    let discovery_doc = serde_json::json!({ "issuer": issuer, "jwks_uri": jwks_uri });
    let jwks_doc = serde_json::json!({ "keys": [jwk] });
    let app = axum::Router::new()
        .route(
            "/.well-known/openid-configuration",
            axum::routing::get(move || {
                let doc = discovery_doc.clone();
                async move { axum::Json(doc) }
            }),
        )
        .route(
            "/.well-known/jwks.json",
            axum::routing::get(move || {
                let doc = jwks_doc.clone();
                async move { axum::Json(doc) }
            }),
        );
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let registry =
        OidcRegistry::from_config(&[IssuerConfig::discovery(issuer.clone())]).expect("registry");

    let mut header = Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some("test-key-1".to_string());
    let token = encode(
        &header,
        &serde_json::json!({
            "iss": issuer,
            "sub": "user_123",
            "kind": "user",
            "team_id": "team_456",
            "exp": future_exp(),
        }),
        &encoding_key,
    )
    .expect("encode RS256 token");

    let claims = registry
        .try_validate(&token)
        .await
        .expect("issuer should match")
        .expect("token should validate");
    assert_eq!(claims.sub, "user_123");

    // A token signed by a different key but claiming the same kid/issuer must
    // be rejected — JWKS lookup succeeds, signature verification fails.
    let attacker_key = rsa::RsaPrivateKey::new(&mut rng, 2048).expect("generate attacker key");
    let attacker_pem =
        rsa::pkcs1::EncodeRsaPrivateKey::to_pkcs1_pem(&attacker_key, rsa::pkcs8::LineEnding::LF)
            .expect("encode attacker PEM");
    let forged = encode(
        &header,
        &serde_json::json!({ "iss": issuer, "sub": "user_123", "exp": future_exp() }),
        &EncodingKey::from_rsa_pem(attacker_pem.as_bytes()).expect("attacker key"),
    )
    .expect("encode forged token");
    let result = registry
        .try_validate(&forged)
        .await
        .expect("issuer should match");
    assert!(result.is_err(), "forged signature must be rejected");
}
