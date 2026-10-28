//! Auth-middleware tests for the federated Chalk JWT admin path: tokens
//! whose `iss` matches a configured Chalk issuer are accepted on the admin
//! surface (the `/v1/admin/*` prefix, kept admin-gated as defence in depth
//! after the HTTP admin routes were removed, and gRPC `/chalk.router.v1.*`)
//! as an alternative to `ROUTER_ADMIN_KEY`.
//!
//! These tests use an HS256 `secret_env` issuer so no JWKS server is needed;
//! the JWKS/OIDC-discovery path is covered by chalk-federated-auth's own
//! test suite.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{self, Request, StatusCode},
    middleware,
    routing::get,
    Router,
};
use chalk_federated_auth::OidcRegistry;
use openai_server::{
    api_keys::{ApiKeyStore, InMemoryApiKeyStore},
    backend::RouterBackend,
    grpc::{build_grpc_router, HealthState},
    hashing::KeyHasher,
    middleware::auth_middleware,
    model_routes::InMemoryModelRouteStore,
    provider_connections::InMemoryProviderConnectionStore,
    AppState,
};
use tower::ServiceExt;

use crate::support::app::AppStateBuilder;
use crate::support::auth::{chalk_jwt, federated_registry_with_audience, user_claims};

const TEST_ISSUER: &str = "https://api.chalk-test.example";
const TEST_SECRET: &str = "federated-test-secret";
const TEST_SECRET_ENV: &str = "FEDERATED_ADMIN_TEST_SECRET";

async fn test_state(federated_auth: Arc<OidcRegistry>, admin_key: Option<&str>) -> AppState {
    let backend = Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        Arc::new(InMemoryProviderConnectionStore::new()),
        Arc::new(InMemoryModelRouteStore::new()),
    ));
    let mut builder = AppStateBuilder::new(backend)
        .api_key_store(Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
            b"test-pepper".to_vec(),
        ))) as Arc<dyn ApiKeyStore>)
        .federated_auth(federated_auth);
    if let Some(key) = admin_key {
        builder = builder.admin_key(key);
    }
    let mut state = builder.build().await;
    state.federated_admin_team_id = Some("team_456".into());
    state.federated_jwt_audience = Some("router-test".into());
    state
}

/// Build a synthetic `/v1/admin/*` route (the real HTTP admin surface was
/// removed; `is_admin_path` still gates the prefix as defence in depth) and
/// the gRPC router, both behind the auth middleware, mirroring the layering
/// in main.rs. Driven with `oneshot` rather than a spawned server: nothing
/// here depends on a real socket.
fn build_test_app(state: AppState) -> Router {
    async fn admin_probe() -> StatusCode {
        StatusCode::OK
    }

    Router::new()
        .route("/v1/admin/keys", get(admin_probe))
        .route("/health", get(admin_probe))
        .route("/v1/healthcheck", get(admin_probe))
        .with_state(state.clone())
        .merge(build_grpc_router(state.clone(), HealthState::new()))
        .layer(middleware::from_fn_with_state(state, auth_middleware))
}

fn registry() -> Arc<OidcRegistry> {
    federated_registry_with_audience(
        TEST_ISSUER,
        TEST_SECRET_ENV,
        TEST_SECRET,
        Some("router-test"),
    )
}

fn claims() -> serde_json::Value {
    let mut claims = user_claims(TEST_ISSUER, "user_123");
    claims["environment_id"] = serde_json::json!("develop");
    claims["aud"] = serde_json::json!("router-test");
    claims["team_permissions"] = serde_json::json!(["tokens.list", "tokens.write"]);
    claims
}

#[tokio::test]
async fn federated_jwt_cannot_access_legacy_admin_paths_without_a_declared_permission() {
    let app = build_test_app(test_state(registry(), None).await);

    let token = chalk_jwt(TEST_SECRET, &claims());
    let request = Request::builder()
        .uri("/v1/admin/keys")
        .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn federated_jwt_grants_grpc_admin_access() {
    let app = build_test_app(test_state(registry(), None).await);

    let token = chalk_jwt(TEST_SECRET, &claims());
    // gRPC-Web frame: 1-byte flags, 4-byte big-endian length, empty payload.
    let request = Request::builder()
        .method(http::Method::POST)
        .uri("/chalk.router.v1.ApiKeyService/ListKeys")
        .header(http::header::CONTENT_TYPE, "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(vec![0u8, 0, 0, 0, 0]))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body).to_string();
    assert!(body.contains("grpc-status:0"), "body: {body:?}");
}

#[tokio::test]
async fn router_api_key_gets_a_clear_control_plane_error() {
    let app = build_test_app(test_state(registry(), None).await);

    // This token is intentionally not seeded in the key store. The rejection
    // classifies the credential type only; it must not reveal whether a router
    // API key exists or has any particular permissions.
    let request = Request::builder()
        .method(http::Method::POST)
        .uri("/chalk.router.v1.ApiKeyService/ListKeys")
        .header(http::header::CONTENT_TYPE, "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        .header(
            http::header::AUTHORIZATION,
            "Bearer ck-00000000000000000000000000000000",
        )
        .body(Body::from(vec![0u8, 0, 0, 0, 0]))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["type"], "permission_error");
    assert_eq!(body["error"]["code"], "access_denied");
    assert_eq!(
        body["error"]["message"],
        "Router API keys cannot access control-plane RPCs; use a Chalk client JWT."
    );
}

#[tokio::test]
async fn forged_federated_jwt_rejected() {
    let app = build_test_app(test_state(registry(), None).await);

    // Correct issuer claim, wrong signing key.
    let token = chalk_jwt("not-the-real-secret", &claims());
    let request = Request::builder()
        .uri("/v1/admin/keys")
        .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn expired_federated_jwt_rejected() {
    let app = build_test_app(test_state(registry(), None).await);

    let mut claims = claims();
    claims["exp"] = serde_json::json!(1);
    let token = chalk_jwt(TEST_SECRET, &claims);
    let request = Request::builder()
        .uri("/v1/admin/keys")
        .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unknown_issuer_token_rejected_with_generic_authentication_message() {
    let app = build_test_app(test_state(registry(), None).await);

    let mut claims = claims();
    claims["iss"] = serde_json::json!("https://not-chalk.example");
    let token = chalk_jwt(TEST_SECRET, &claims);
    let request = Request::builder()
        .uri("/v1/admin/keys")
        .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error"]["message"],
        "Authentication failed"
    );
}

#[tokio::test]
async fn admin_key_still_works_alongside_federation() {
    let app = build_test_app(test_state(registry(), Some("test-admin-key")).await);

    let request = Request::builder()
        .uri("/v1/admin/keys")
        .header(http::header::AUTHORIZATION, "Bearer test-admin-key")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn no_admin_auth_configured_returns_forbidden() {
    let app = build_test_app(test_state(Arc::new(OidcRegistry::empty()), None).await);

    let token = chalk_jwt(TEST_SECRET, &claims());
    let request = Request::builder()
        .uri("/v1/admin/keys")
        .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn http_health_endpoints_stay_public() {
    let app = build_test_app(test_state(registry(), None).await);
    for path in ["/health", "/v1/healthcheck"] {
        let request = Request::builder().uri(path).body(Body::empty()).unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
}

#[tokio::test]
async fn standard_health_service_stays_public() {
    let app = build_test_app(test_state(registry(), None).await);

    let request = Request::builder()
        .method(http::Method::POST)
        .uri("/grpc.health.v1.Health/Check")
        .header(http::header::CONTENT_TYPE, "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        // HealthCheckRequest { service: "readiness" } in a gRPC-Web frame.
        .body(Body::from(vec![
            0u8, 0, 0, 0, 11, 10, 9, b'r', b'e', b'a', b'd', b'i', b'n', b'e', b's', b's',
        ]))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

fn rpc_request(path: &str, token: &str, selector: Option<&str>, create: bool) -> Request<Body> {
    let mut request = Request::post(path)
        .header(http::header::CONTENT_TYPE, "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        .header(http::header::AUTHORIZATION, format!("Bearer {token}"));
    if let Some(selector) = selector {
        request = request.header("x-chalk-env-id", selector);
    }
    // CreateKeyRequest { description: "audit" }, or an empty read request.
    request
        .body(Body::from(if create {
            vec![0u8, 0, 0, 0, 7, 10, 5, b'a', b'u', b'd', b'i', b't']
        } else {
            vec![0u8; 5]
        }))
        .unwrap()
}

#[tokio::test]
async fn every_admin_rpc_rejects_unprivileged_or_wrong_scope_jwts() {
    use openai_server::cluster_auth::ADMIN_OPERATION_PERMISSIONS;
    use serde_json::json;
    let mut state = test_state(registry(), None).await;
    // This compatibility flag must NEVER relax admin authorization.
    state.accept_cross_environment_tokens = true;
    let app = build_test_app(state);
    let mut cases = Vec::new();
    for kind in ["user", "service-account", "sandbox", "tenant"] {
        let mut payload = claims();
        payload["kind"] = json!(kind);
        payload["team_permissions"] = json!(["query.online"]);
        payload["permissions"] = json!(["query.online"]);
        payload["env_permissions"] = json!({"develop":["query.online"]});
        cases.push((kind.to_string(), payload, StatusCode::FORBIDDEN));
    }
    for (name, field, value) in [
        ("missing environment", "environment_id", json!(null)),
        ("empty environment", "environment_id", json!("")),
        ("wrong environment", "environment_id", json!("other")),
        ("missing team", "team_id", json!(null)),
        ("wrong team", "team_id", json!("other-team")),
        (
            "wrong impersonated team",
            "impersonated_team_id",
            json!("other-team"),
        ),
        ("unknown kind", "kind", json!("exchange")),
        ("empty grants", "team_permissions", json!([])),
    ] {
        let mut payload = claims();
        payload[field] = value;
        cases.push((name.to_string(), payload, StatusCode::FORBIDDEN));
    }
    for (name, field, value) in [
        ("wrong audience", "aud", json!("another-service")),
        ("missing audience", "aud", json!(null)),
        ("empty audience", "aud", json!("")),
        ("empty audience list", "aud", json!([])),
        (
            "malformed permissions",
            "permissions",
            json!("tokens.write"),
        ),
        ("unsupported compact permissions", "p", json!({"a":[]})),
    ] {
        let mut payload = claims();
        payload[field] = value;
        cases.push((name.to_string(), payload, StatusCode::UNAUTHORIZED));
    }
    for (name, payload, expected) in cases {
        let token = chalk_jwt(TEST_SECRET, &payload);
        for (path, _) in ADMIN_OPERATION_PERMISSIONS {
            let response = app
                .clone()
                .oneshot(rpc_request(path, &token, None, false))
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{name}: {path}");
        }
    }
    let token = chalk_jwt(TEST_SECRET, &claims());
    for selector in ["other", ""] {
        for (path, _) in ADMIN_OPERATION_PERMISSIONS {
            let response = app
                .clone()
                .oneshot(rpc_request(path, &token, Some(selector), false))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "selector={selector:?}: {path}"
            );
        }
    }
}

#[tokio::test]
async fn explicit_team_and_environment_grants_can_create_keys() {
    use serde_json::json;
    for kind in ["user", "service-account", "sandbox"] {
        let mut payload = claims();
        payload["kind"] = json!(kind);
        payload["team_permissions"] = json!([]);
        payload["permissions"] = json!(["tokens.write"]);
        payload["env_permissions"] = json!({"develop":["tokens.write"]});
        let app = build_test_app(test_state(registry(), None).await);
        let token = chalk_jwt(TEST_SECRET, &payload);
        let response = app
            .oneshot(rpc_request(
                "/chalk.router.v1.ApiKeyService/CreateKey",
                &token,
                None,
                true,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{kind}");
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(
            body.contains("grpc-status:0") && body.contains("ck-"),
            "{kind}: {body}"
        );
    }
    let app = build_test_app(test_state(registry(), None).await);
    let token = chalk_jwt(TEST_SECRET, &claims());
    let response = app
        .oneshot(rpc_request(
            "/chalk.router.v1.ApiKeyService/CreateKey",
            &token,
            None,
            true,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("grpc-status:0"));
}

#[tokio::test]
async fn cached_read_token_cannot_write_or_access_unknown_admin_paths() {
    let mut payload = claims();
    payload["team_permissions"] = serde_json::json!(["tokens.list"]);
    let token = chalk_jwt(TEST_SECRET, &payload);
    let app = build_test_app(test_state(registry(), None).await);
    let response = app
        .clone()
        .oneshot(rpc_request(
            "/chalk.router.v1.ApiKeyService/ListKeys",
            &token,
            None,
            false,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    for path in [
        "/chalk.router.v1.ApiKeyService/CreateKey",
        "/chalk.router.v1.UnknownService/Get",
        "/v1/debug/traces",
    ] {
        let response = app
            .clone()
            .oneshot(rpc_request(path, &token, None, true))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
}

#[tokio::test]
async fn jwt_admin_fails_closed_without_a_configured_team() {
    let mut state = test_state(registry(), None).await;
    state.federated_admin_team_id = None;
    let app = build_test_app(state);
    let token = chalk_jwt(TEST_SECRET, &claims());
    let response = app
        .oneshot(rpc_request(
            "/chalk.router.v1.ApiKeyService/CreateKey",
            &token,
            None,
            true,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[test]
fn federation_requires_audience_and_environment_owner_at_startup() {
    use openai_server::config::{ClusterConfig, RouterConfig};
    let mut config = RouterConfig::from_env();
    config.cluster = None;
    config.chalk_jwt_issuers = Some(TEST_ISSUER.into());
    config.federated_admin_team_id = Some("team_456".into());
    for audience in [None, Some("".into()), Some(" ".into())] {
        config.chalk_jwt_audience = audience;
        assert!(config
            .federated_issuer_configs()
            .unwrap_err()
            .contains("CHALK_JWT_AUDIENCE"));
    }
    config.chalk_jwt_audience = Some("router-test".into());
    for team in [None, Some("".into()), Some(" ".into())] {
        config.federated_admin_team_id = team;
        assert!(config
            .federated_issuer_configs()
            .unwrap_err()
            .contains("CHALK_TEAM_ID"));
    }
    config.federated_admin_team_id = Some("team_456".into());
    assert_eq!(
        config.federated_issuer_configs().unwrap()[0]
            .audience
            .as_deref(),
        Some("router-test")
    );
    config.cluster = Some(ClusterConfig {
        metadata_api_endpoint: "http://localhost".into(),
        metadata_api_token: "test".into(),
        kube_cluster_id: None,
    });
    config.federated_admin_team_id = None;
    assert!(config.federated_issuer_configs().is_ok());
    config.cluster = None;
    config.chalk_jwt_issuers = None;
    config.chalk_jwt_audience = None;
    assert!(config.federated_issuer_configs().unwrap().is_empty());
}

#[tokio::test]
async fn audience_policy_is_enforced_on_cached_tokens_and_accepts_audience_lists() {
    let mut state = test_state(registry(), None).await;
    let mut payload = claims();
    payload["aud"] = serde_json::json!(["another-service", "router-test"]);
    let token = chalk_jwt(TEST_SECRET, &payload);
    let path = "/chalk.router.v1.ApiKeyService/ListKeys";
    let response = build_test_app(state.clone())
        .oneshot(rpc_request(path, &token, None, false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // Same registry and token, different runtime audience: do not reuse the old result.
    state.federated_jwt_audience = Some("other-router".into());
    let response = build_test_app(state)
        .oneshot(rpc_request(path, &token, None, false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
