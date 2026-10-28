//! The development escape hatch permits missing proxy credentials only.

use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    middleware,
    routing::get,
    Json, Router,
};
use openai_server::{
    grpc::{build_grpc_router, HealthState},
    middleware::{auth_middleware, ResolvedApiKey},
};
use tower::ServiceExt;

use crate::support::app::{AppStateBuilder, TEST_API_KEY};

async fn probe(key: ResolvedApiKey) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "prefix": key.0.key_prefix,
        "principal": key.authenticated_principal_id(),
    }))
}

async fn app(permit: bool, admin_key: Option<&str>) -> Router {
    let mut builder = AppStateBuilder::new(Arc::new(super::span_test::MockBackend))
        .dangerously_permit_unauthenticated(permit);
    if let Some(key) = admin_key {
        builder = builder.admin_key(key);
    }
    let state = builder.build().await;
    Router::new()
        .route("/v1/models", get(probe))
        .route("/v1beta/interactions", get(probe))
        .with_state(state.clone())
        .merge(build_grpc_router(state.clone(), HealthState::new()))
        .layer(middleware::from_fn_with_state(state, auth_middleware))
}

#[tokio::test]
async fn anonymous_proxy_access_requires_explicit_opt_in() {
    for (permit, expected) in [(false, StatusCode::UNAUTHORIZED), (true, StatusCode::OK)] {
        // A configured admin key must not interfere with opting into local
        // anonymous proxy access (just run loads one for the admin UI).
        let response = app(permit, Some("local-admin"))
            .await
            .oneshot(Request::get("/v1/models").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        if permit {
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["prefix"], "anonymous");
            assert!(body["principal"].is_null());
        }
    }
}

#[tokio::test]
async fn development_mode_still_validates_presented_credentials() {
    let app = app(true, None).await;
    for (path, name, value) in [
        ("/v1/models", "authorization", "Bearer invalid"),
        ("/v1/models", "authorization", "Bearer "),
        ("/v1/models", "authorization", "Basic invalid"),
        ("/v1/models", "x-api-key", "invalid"),
        ("/v1/models", "x-api-key", ""),
        ("/v1beta/interactions", "x-goog-api-key", "invalid"),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::get(path)
                    .header(name, value)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path} {name}");
    }

    let response = app
        .oneshot(
            Request::get("/v1/models")
                .header("x-api-key", TEST_API_KEY)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_ne!(body["prefix"], "anonymous");
    assert!(body["principal"].is_string());
}

#[tokio::test]
async fn development_mode_keeps_admin_rpcs_authenticated() {
    for (admin_key, presented_key, expected) in [
        (None, None, StatusCode::FORBIDDEN),
        (Some("local-admin"), None, StatusCode::UNAUTHORIZED),
        (
            Some("local-admin"),
            Some("invalid"),
            StatusCode::UNAUTHORIZED,
        ),
        (
            Some("local-admin"),
            Some(TEST_API_KEY),
            StatusCode::UNAUTHORIZED,
        ),
        (Some("local-admin"), Some("local-admin"), StatusCode::OK),
    ] {
        let mut request = Request::post("/chalk.router.v1.ApiKeyService/ListKeys")
            .header(header::CONTENT_TYPE, "application/grpc-web+proto")
            .header("x-grpc-web", "1");
        if let Some(key) = presented_key {
            request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
        }
        let response = app(true, admin_key)
            .await
            .oneshot(request.body(Body::from(vec![0u8; 5])).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
}

#[tokio::test]
async fn authentication_storage_failure_returns_unavailable_instead_of_invalid_key() {
    use openai_server::{api_keys::PostgresApiKeyStore, hashing::KeyHasher};

    // No database is needed: a closed pool must be surfaced as unavailable,
    // rather than being mistaken for a missing API-key row.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://test:test@127.0.0.1/unused")
        .unwrap();
    pool.close().await;
    let store = Arc::new(PostgresApiKeyStore::new(
        pool,
        KeyHasher::new(b"unavailable-auth-test".to_vec()),
        "test".to_string(),
    ));
    let state = AppStateBuilder::new(Arc::new(super::span_test::MockBackend))
        .api_key_store(store)
        .build()
        .await;
    let app = Router::new()
        .route("/v1/models", get(probe))
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, auth_middleware));
    let response = app
        .oneshot(
            Request::get("/v1/models")
                .header("x-api-key", "presented-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["type"], "server_error");
    assert_eq!(
        body["error"]["message"],
        "Authentication temporarily unavailable"
    );
}
