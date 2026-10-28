//! Auth-middleware tests for the federated Chalk JWT *proxied* path: tokens
//! whose `iss` matches a configured Chalk issuer are accepted on the
//! non-admin (OpenAI/Anthropic-compatible) surface as an alternative to a
//! stored API key — this is what lets api-server-go forward a user's Chalk
//! JWT straight through.
//!
//! Like the admin tests, these use an HS256 `secret_env` issuer so no JWKS
//! server is needed; the JWKS/OIDC-discovery path is covered by
//! chalk-federated-auth's own test suite.

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
    fallback::FallbackPolicy,
    hashing::KeyHasher,
    middleware::auth_middleware,
    model_routes::InMemoryModelRouteStore,
    provider_connections::InMemoryProviderConnectionStore,
    AppState,
};
use tower::ServiceExt;

use crate::support::app::{build_app_with_federated_auth, chat_request_body, AppStateBuilder};
use crate::support::auth::{chalk_jwt, federated_registry, user_claims};
use crate::support::otel::{setup_otel, span_attrs};

const TEST_ISSUER: &str = "be";
const TEST_SECRET: &str = "federated-proxy-test-secret";
const TEST_SECRET_ENV: &str = "FEDERATED_PROXY_TEST_SECRET";

fn registry() -> Arc<OidcRegistry> {
    federated_registry(TEST_ISSUER, TEST_SECRET_ENV, TEST_SECRET)
}

fn claims() -> serde_json::Value {
    user_claims(TEST_ISSUER, "user_123")
}

async fn test_state(federated_auth: Arc<OidcRegistry>) -> AppState {
    let backend = Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        Arc::new(InMemoryProviderConnectionStore::new()),
        Arc::new(InMemoryModelRouteStore::new()),
    ));
    AppStateBuilder::new(backend)
        .api_key_store(Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
            b"test-pepper".to_vec(),
        ))) as Arc<dyn ApiKeyStore>)
        .federated_auth(federated_auth)
        .build()
        .await
}

/// Build a non-admin route behind the auth middleware. The route returns 200
/// only if the request passes auth, so a 200 means federation accepted the
/// token. Missing or unrecognized credentials are rejected. Driven with
/// `oneshot` rather than a spawned server: nothing here depends on a real socket.
fn build_test_app(state: AppState) -> Router {
    Router::new()
        .route("/v1/models", get(|| async { "ok" }))
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, auth_middleware))
}

#[tokio::test]
async fn federated_jwt_grants_proxied_access() {
    let app = build_test_app(test_state(registry()).await);

    let token = chalk_jwt(TEST_SECRET, &claims());
    let request = Request::builder()
        .uri("/v1/models")
        .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn forged_federated_jwt_rejected_on_proxy() {
    let app = build_test_app(test_state(registry()).await);

    // Correct issuer claim, wrong signing key.
    let token = chalk_jwt("not-the-real-secret", &claims());
    let request = Request::builder()
        .uri("/v1/models")
        .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn plain_token_falls_through_to_api_key_store() {
    let app = build_test_app(test_state(registry()).await);

    // A non-JWT token has no `iss` -> federation returns None -> the request
    // falls through to the API-key store, which doesn't have it -> 401. This
    // proves federation didn't break the existing API-key path.
    let request = Request::builder()
        .uri("/v1/models")
        .header(http::header::AUTHORIZATION, "Bearer sk-not-a-real-key")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn federated_jwt_sub_is_stamped_as_authenticated_principal() {
    let (provider, exporter, subscriber) = setup_otel();
    let app = build_app_with_federated_auth(
        Arc::new(super::span_test::MockBackend),
        FallbackPolicy::empty(),
        registry(),
    )
    .await;
    let token = chalk_jwt(TEST_SECRET, &claims());

    let response = {
        let _guard = tracing::subscriber::set_default(subscriber);
        let request = Request::builder()
            .method(http::Method::POST)
            .uri("/v1/chat/completions")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(
                serde_json::to_vec(&chat_request_body(false)).unwrap(),
            ))
            .unwrap();

        app.oneshot(request).await.unwrap()
    };
    assert_eq!(response.status(), StatusCode::OK);

    for result in provider.force_flush() {
        result.expect("flush should succeed");
    }

    let spans = exporter.get_finished_spans().unwrap();
    let span = spans
        .iter()
        .find(|span| span.name.as_ref() == "chat.completion")
        .expect("expected a chat completion span");
    let attributes = span_attrs(span);
    assert_eq!(
        attributes
            .get("chalk.auth.principal.id")
            .map(String::as_str),
        Some("user_123")
    );
    assert_eq!(
        attributes
            .get("chalk.auth.principal.type")
            .map(String::as_str),
        Some("service_token")
    );
    assert_eq!(
        attributes.get("user.id").map(String::as_str),
        Some("user_123")
    );
}
