//! Integration tests for `POST /v1/traces/spans`.
//!
//! Unlike `span_test.rs` (which captures spans created via the `tracing`
//! layer), the ingest endpoint emits spans directly through the *global*
//! OpenTelemetry tracer provider, so these tests install a global provider
//! backed by an `InMemorySpanExporter`. All cases share one test body: the
//! global provider is process-wide state and must be installed exactly once.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::Body,
    http::{self, Request, StatusCode},
    middleware,
    routing::post,
    Router,
};
use http_body_util::BodyExt;
use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
use opentelemetry_sdk::trace::{SimpleSpanProcessor, TracerProvider};
use tower::ServiceExt;

use openai_server::{
    api_keys::{ApiKey, ApiKeyPermission, ApiKeyStore, InMemoryApiKeyStore},
    handlers,
    hashing::KeyHasher,
    middleware::auth_middleware,
};

use crate::support::app::AppStateBuilder;

const TEST_API_KEY: &str = "test-key-for-span-ingest";

const TRACE_ID: &str = "0af7651916cd43dd8448eb211c80319c";
const SESSION_SPAN_ID: &str = "b7ad6b7169203331";
const TURN_SPAN_ID: &str = "00f067aa0ba902b7";
const TOOL_SPAN_ID: &str = "53995c3f42cd8ad8";
const JWT_ISSUER: &str = "span-ingest";
const JWT_SECRET: &str = "span-ingest-secret";

/// A recent timestamp: ingest rejects spans older than its maximum age.
fn recent_unix_nanos() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    (now - std::time::Duration::from_secs(600)).as_nanos() as u64
}

async fn build_app() -> Router {
    let hasher = KeyHasher::new(b"test-pepper".to_vec());
    let api_key_store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(hasher.clone()));
    // Seed a test API key so auth middleware succeeds. authenticate() hashes the
    // presented token, so the seeded key_hash must be hash(TEST_API_KEY).
    api_key_store
        .insert_raw(ApiKey {
            key_hash: hasher.hash(TEST_API_KEY),
            key_prefix: openai_server::hashing::key_prefix(TEST_API_KEY),
            description: "test key".to_string(),
            created_at: chrono::Utc::now(),
            permission: ApiKeyPermission::default(),
            labels: HashMap::new(),
            usage_pool_id: None,
            daily_token_budget: None,
        })
        .await
        .expect("seed test key");

    let backend = Arc::new(openai_server::backend::OpenAICompatibleBackend::new(
        reqwest::Client::new(),
        "unused".to_string(),
    ));
    let state = AppStateBuilder::new(backend)
        .api_key_store(api_key_store)
        .federated_auth(crate::support::auth::federated_registry(
            JWT_ISSUER,
            "SPAN_INGEST_JWT_SECRET",
            JWT_SECRET,
        ))
        .build()
        .await;

    Router::new()
        .route("/v1/traces/spans", post(handlers::traces::ingest_spans))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

async fn post_spans(app: &Router, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    post_spans_as(app, TEST_API_KEY, body).await
}

async fn post_spans_as(
    app: &Router,
    credential: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(http::Method::POST)
        .uri("/v1/traces/spans")
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, format!("Bearer {credential}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::json!({}));
    (status, json)
}

fn attrs(span: &opentelemetry_sdk::export::trace::SpanData) -> HashMap<String, String> {
    span.attributes
        .iter()
        .map(|kv| (kv.key.to_string(), kv.value.to_string()))
        .collect()
}

#[tokio::test]
async fn test_span_ingest_end_to_end() {
    openai_server::handlers::init_content_logging(true);
    // Install the global provider once for the whole test.
    let exporter = InMemorySpanExporter::default();
    let provider = TracerProvider::builder()
        .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
        .build();
    opentelemetry::global::set_tracer_provider(provider.clone());

    let app = build_app().await;

    // --- happy path: session root + turn child + tool grandchild -----------
    let start_ns: u64 = recent_unix_nanos();
    let (status, body) = post_spans(
        &app,
        serde_json::json!({
            "spans": [
                {
                    "name": "opencode.session",
                    "kind": "session",
                    "trace_id": TRACE_ID,
                    "span_id": SESSION_SPAN_ID,
                    "start_time_unix_nano": start_ns,
                    "end_time_unix_nano": start_ns + 60_000_000_000u64,
                    "attributes": {
                        "session.id": "ses_123",
                        "input.value": "fix the bug",
                    },
                },
                {
                    "name": "opencode.turn",
                    "kind": "turn",
                    "trace_id": TRACE_ID,
                    "span_id": TURN_SPAN_ID,
                    "parent_span_id": SESSION_SPAN_ID,
                    // string timestamps must be accepted (JS can't send u64)
                    "start_time_unix_nano": start_ns.to_string(),
                    "end_time_unix_nano": (start_ns + 30_000_000_000u64).to_string(),
                    "attributes": { "session.id": "ses_123" },
                },
                {
                    "name": "tool: bash",
                    "kind": "tool",
                    "trace_id": TRACE_ID,
                    "span_id": TOOL_SPAN_ID,
                    "parent_span_id": TURN_SPAN_ID,
                    "start_time_unix_nano": start_ns + 1_000_000_000u64,
                    "end_time_unix_nano": start_ns + 3_500_000_000u64,
                    "attributes": {
                        "tool.name": "bash",
                        "input.value": "{\"command\":\"ls\"}",
                        "output.value": "README.md",
                    },
                    "status": { "code": "ok" },
                },
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["accepted"], 3);

    provider.force_flush();
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 3, "expected 3 exported spans, got {spans:?}");

    let by_name: HashMap<&str, &opentelemetry_sdk::export::trace::SpanData> =
        spans.iter().map(|s| (s.name.as_ref(), s)).collect();

    let session = by_name["opencode.session"];
    assert_eq!(session.span_context.trace_id().to_string(), TRACE_ID);
    assert_eq!(session.span_context.span_id().to_string(), SESSION_SPAN_ID);
    assert_eq!(
        session.parent_span_id,
        opentelemetry::trace::SpanId::INVALID
    );
    let a = attrs(session);
    assert_eq!(
        a.get("openinference.span.kind").map(String::as_str),
        Some("AGENT")
    );
    assert_eq!(a.get("session.id").map(String::as_str), Some("ses_123"));

    let turn = by_name["opencode.turn"];
    assert_eq!(turn.span_context.trace_id().to_string(), TRACE_ID);
    assert_eq!(turn.span_context.span_id().to_string(), TURN_SPAN_ID);
    assert_eq!(turn.parent_span_id.to_string(), SESSION_SPAN_ID);
    assert_eq!(
        attrs(turn)
            .get("openinference.span.kind")
            .map(String::as_str),
        Some("CHAIN")
    );

    let tool = by_name["tool: bash"];
    assert_eq!(tool.parent_span_id.to_string(), TURN_SPAN_ID);
    let a = attrs(tool);
    assert_eq!(
        a.get("openinference.span.kind").map(String::as_str),
        Some("TOOL")
    );
    assert_eq!(a.get("tool.name").map(String::as_str), Some("bash"));
    // Duration must be preserved from the client timestamps (2.5s).
    let duration = tool.end_time.duration_since(tool.start_time).unwrap();
    assert_eq!(duration, std::time::Duration::from_millis(2500));

    // Every ingested span carries the router's provenance marker and the
    // verified caller. A router API key is a principal but never a user.
    let key_principal = KeyHasher::new(b"test-pepper".to_vec()).hash(TEST_API_KEY);
    for span in &spans {
        let a = attrs(span);
        assert_eq!(
            a.get("chalk.span.source").map(String::as_str),
            Some("client")
        );
        assert_eq!(
            a.get("chalk.auth.principal.type").map(String::as_str),
            Some("router_api_key")
        );
        assert_eq!(
            a.get("chalk.auth.principal.id"),
            Some(&key_principal),
            "{}",
            span.name
        );
        assert!(!a.contains_key("user.id"));
    }

    // --- forged identity is replaced by the verified one --------------------
    exporter.reset();
    let forged = |span_id: &str| {
        serde_json::json!({
            "name": "forged",
            "kind": "tool",
            "trace_id": TRACE_ID,
            "span_id": span_id,
            "start_time_unix_nano": start_ns,
            "end_time_unix_nano": start_ns + 1_000_000_000u64,
            "attributes": {
                "user.id": "someone-else",
                "chalk.auth.principal.id": "someone-else",
                "chalk.auth.principal.type": "service_token",
                "chalk.span.source": "router",
            },
        })
    };
    let (status, _) = post_spans(&app, serde_json::json!({"spans": [forged(TOOL_SPAN_ID)]})).await;
    assert_eq!(status, StatusCode::OK);
    provider.force_flush();
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 1);
    let a = attrs(&spans[0]);
    assert_eq!(a.get("chalk.auth.principal.id"), Some(&key_principal));
    assert_eq!(
        a.get("chalk.auth.principal.type").map(String::as_str),
        Some("router_api_key")
    );
    assert_eq!(
        a.get("chalk.span.source").map(String::as_str),
        Some("client")
    );
    assert!(!a.contains_key("user.id"));
    assert!(spans[0]
        .attributes
        .iter()
        .all(|kv| kv.value.as_str() != "someone-else"));

    // --- a federated user is stamped as the principal and the user ----------
    exporter.reset();
    let token = crate::support::auth::chalk_jwt(
        JWT_SECRET,
        &serde_json::json!({"iss": JWT_ISSUER, "sub": "user-42", "exp": crate::support::auth::future_exp()}),
    );
    let (status, body) = post_spans_as(
        &app,
        &token,
        serde_json::json!({"spans": [forged(TURN_SPAN_ID)]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    provider.force_flush();
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 1);
    let a = attrs(&spans[0]);
    assert_eq!(
        a.get("chalk.auth.principal.id").map(String::as_str),
        Some("user-42")
    );
    assert_eq!(a.get("user.id").map(String::as_str), Some("user-42"));
    assert_eq!(
        a.get("chalk.span.source").map(String::as_str),
        Some("client")
    );

    // --- implausible timestamps are a 400 and nothing is exported -----------
    exporter.reset();
    let (status, body) = post_spans(
        &app,
        serde_json::json!({
            "spans": [{
                "name": "ancient",
                "kind": "tool",
                "trace_id": TRACE_ID,
                "span_id": TOOL_SPAN_ID,
                "start_time_unix_nano": 1,
                "end_time_unix_nano": 2,
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]["message"].as_str().unwrap().contains("old"),
        "body: {body}"
    );
    provider.force_flush();
    assert!(exporter.get_finished_spans().unwrap().is_empty());

    // --- validation: bad ids are a 400 and nothing is exported --------------
    exporter.reset();
    let (status, body) = post_spans(
        &app,
        serde_json::json!({
            "spans": [{
                "name": "bad",
                "kind": "tool",
                "trace_id": "zz",
                "span_id": TOOL_SPAN_ID,
                "start_time_unix_nano": 1,
                "end_time_unix_nano": 2,
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("trace_id"),
        "body: {body}"
    );
    provider.force_flush();
    assert!(exporter.get_finished_spans().unwrap().is_empty());

    // --- a batch with one invalid span is rejected atomically ---------------
    exporter.reset();
    let (status, _) = post_spans(
        &app,
        serde_json::json!({
            "spans": [
                {
                    "name": "good",
                    "kind": "turn",
                    "trace_id": TRACE_ID,
                    "span_id": TURN_SPAN_ID,
                    "start_time_unix_nano": start_ns,
                    "end_time_unix_nano": start_ns + 1,
                },
                {
                    "name": "bad",
                    "kind": "turn",
                    "trace_id": TRACE_ID,
                    "span_id": "0000000000000000",
                    "start_time_unix_nano": 1,
                    "end_time_unix_nano": 2,
                },
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    provider.force_flush();
    assert!(exporter.get_finished_spans().unwrap().is_empty());

    // --- auth is required ----------------------------------------------------
    let req = Request::builder()
        .method(http::Method::POST)
        .uri("/v1/traces/spans")
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"spans":[]}"#))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
