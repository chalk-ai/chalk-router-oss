//! Auth wiring for Anthropic Messages and token counting through the real
//! middleware and required `ResolvedApiKey` extractor. Authenticated requests
//! reach connection resolution (503 with empty stores); missing or invalid
//! credentials fail before dispatch. No request reaches an upstream provider.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{self, Request, StatusCode},
    middleware,
    routing::post,
    Router,
};
use tower::ServiceExt;

use openai_server::{
    backend::RouterBackend,
    handlers,
    middleware::{auth_middleware, ResolvedApiKey},
    model_routes::InMemoryModelRouteStore,
    provider_connections::InMemoryProviderConnectionStore,
};

use crate::support::app::{AppStateBuilder, TEST_API_KEY};

/// A stand-in for chat/embeddings/images: a handler with a *required*
/// `ResolvedApiKey` extractor. It returns the resolved key's prefix so a test
/// can confirm the middleware supplied the authenticated key.
async fn probe(api_key: ResolvedApiKey) -> String {
    api_key.0.key_prefix.clone()
}

/// Build an app with the two Anthropic pass-through routes behind the real auth
/// middleware. The backend uses empty stores, so connection resolution fails
/// without any network call.
async fn build_app() -> Router {
    build_app_with_anonymous(false).await
}

async fn build_app_with_anonymous(permit: bool) -> Router {
    let conns = Arc::new(InMemoryProviderConnectionStore::new());
    let routes = Arc::new(InMemoryModelRouteStore::new());

    // `reqwest::Client::new()` for the backend's http_client is unused (these
    // tests 503 before any outbound call), and is safe here because the crate
    // disables reqwest's `system-proxy` feature, so no macOS System
    // Configuration probe runs — see the reqwest dep note in Cargo.toml.
    let backend = Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        conns.clone(),
        routes.clone(),
    ));
    let state = AppStateBuilder::new(backend)
        .dangerously_permit_unauthenticated(permit)
        .provider_connection_store(conns)
        .model_route_store(routes)
        .build()
        .await;

    Router::new()
        .route("/v1/messages", post(handlers::anthropic::proxy_messages))
        .route(
            "/v1/messages/count_tokens",
            post(handlers::anthropic::proxy_messages_count_tokens),
        )
        // A required-extractor route, standing in for chat/embeddings/images.
        .route("/probe", post(probe))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

async fn post_request(app: &Router, uri: &str, key: Option<&str>) -> StatusCode {
    let mut req = Request::builder()
        .method(http::Method::POST)
        .uri(uri)
        .header(http::header::CONTENT_TYPE, "application/json");
    if let Some(key) = key {
        req = req.header(http::header::AUTHORIZATION, format!("Bearer {key}"));
    }
    let req = req
        .body(Body::from(r#"{"model":"claude-3-5-sonnet","messages":[]}"#))
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn authenticated_request_reaches_anthropic_handlers() {
    let app = build_app().await;
    for path in ["/v1/messages", "/v1/messages/count_tokens"] {
        assert_eq!(
            post_request(&app, path, Some(TEST_API_KEY)).await,
            StatusCode::SERVICE_UNAVAILABLE,
            "{path} should reach connection resolution"
        );
    }
}

#[tokio::test]
async fn anonymous_development_requests_reach_handlers_and_extractors() {
    let app = build_app_with_anonymous(true).await;
    for path in ["/v1/messages", "/v1/messages/count_tokens"] {
        assert_eq!(
            post_request(&app, path, None).await,
            StatusCode::SERVICE_UNAVAILABLE,
            "{path} should reach connection resolution without a credential"
        );
    }
    let response = app
        .oneshot(Request::post("/probe").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body[..], b"anonymous");
}

#[tokio::test]
async fn missing_or_invalid_credentials_are_rejected() {
    let app = build_app().await;
    for path in ["/v1/messages", "/v1/messages/count_tokens", "/probe"] {
        for key in [None, Some("invalid-key"), Some("")] {
            assert_eq!(
                post_request(&app, path, key).await,
                StatusCode::UNAUTHORIZED,
                "{path} must reject missing or invalid credentials"
            );
        }
    }
}

#[tokio::test]
async fn required_extractor_handler_gets_authenticated_key() {
    let app = build_app().await;
    let req = Request::builder()
        .method(http::Method::POST)
        .uri("/probe")
        .header("x-api-key", TEST_API_KEY)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        &body[..],
        openai_server::hashing::key_prefix(TEST_API_KEY).as_bytes()
    );
}
