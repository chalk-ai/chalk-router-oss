//! Exact-prefix caching on the Anthropic `/v1/messages` passthrough.
//!
//! This surface caches the upstream bytes verbatim — a JSON body or a whole SSE
//! stream — so these assert on the *upstream call count* (a mock counts them)
//! rather than only on headers: a hit means api.anthropic.com was not called.
//!
//! Every scenario runs sequentially inside one `#[tokio::test]` because they
//! share one cache and assert on a running upstream call count. The upstream is
//! reached through the seeded connection's `base_url`, so nothing here touches
//! process-global state.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Body,
    extract::State,
    http::{self, Request, StatusCode},
    middleware,
    response::Response,
    routing::post,
    Json, Router,
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use openai_server::{
    api_keys::{ApiKey, ApiKeyPermission, ApiKeyStore, InMemoryApiKeyStore},
    backend::RouterBackend,
    handlers,
    hashing::KeyHasher,
    middleware::auth_middleware,
    model_routes::InMemoryModelRouteStore,
    prompt_cache::{memory::MemoryPromptCache, CachePolicy, CacheScope},
    provider_connections::{
        ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore, ProviderConnectionStore,
    },
};

const TEST_API_KEY: &str = "test-key-for-anthropic-prompt-cache";

// ---------------------------------------------------------------------------
// Mock upstream — counts calls and branches on the request's `stream` field.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MockState {
    messages_calls: AtomicUsize,
    count_tokens_calls: AtomicUsize,
}

const SSE_EVENTS: &[(&str, &str)] = &[
    (
        "message_start",
        r#"{"type":"message_start","message":{"id":"msg_pc","type":"message","role":"assistant","model":"claude-haiku-4-5","content":[],"usage":{"input_tokens":5,"output_tokens":0}}}"#,
    ),
    (
        "content_block_start",
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    ),
    (
        "content_block_delta",
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello from the mock!"}}"#,
    ),
    (
        "content_block_stop",
        r#"{"type":"content_block_stop","index":0}"#,
    ),
    (
        "message_delta",
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}"#,
    ),
    ("message_stop", r#"{"type":"message_stop"}"#),
];

/// The beta whose opt-in adds `input_transformations` to every response — the
/// report of which thinking blocks the API dropped from the conversation.
const THINKING_BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";

async fn handle_messages(
    State(state): State<Arc<MockState>>,
    headers: http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    state.messages_calls.fetch_add(1, Ordering::SeqCst);

    // Mirror the real API: opting into the thinking-binding beta adds an
    // `input_transformations` array to the response. Nothing in the request
    // *body* distinguishes the two calls, which is exactly why the header has
    // to be part of the cache key.
    let opted_in = headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.split(',').any(|b| b.trim() == THINKING_BINDING_BETA));

    if body["stream"].as_bool().unwrap_or(false) {
        let sse: String = SSE_EVENTS
            .iter()
            .map(|(name, data)| format!("event: {name}\ndata: {data}\n\n"))
            .collect();
        return Response::builder()
            .status(StatusCode::OK)
            .header(http::header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(sse))
            .unwrap();
    }
    let mut json = json!({
        "id": "msg_pc",
        "type": "message",
        "role": "assistant",
        "model": "claude-haiku-4-5",
        "content": [{"type": "text", "text": "Hello from the mock!"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 4},
    });
    if opted_in {
        json["input_transformations"] = json!([
            {"type": "thinking_block_dropped", "reason": "prefix_mismatch", "index": 0}
        ]);
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&json).unwrap()))
        .unwrap()
}

async fn handle_count_tokens(
    State(state): State<Arc<MockState>>,
    Json(_): Json<Value>,
) -> Response {
    state.count_tokens_calls.fetch_add(1, Ordering::SeqCst);
    Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"input_tokens":5}"#))
        .unwrap()
}

async fn start_mock() -> (String, Arc<MockState>) {
    let state = Arc::new(MockState::default());
    let app = Router::new()
        .route("/v1/messages", post(handle_messages))
        .route("/v1/messages/count_tokens", post(handle_count_tokens))
        .with_state(state.clone());
    let base = crate::support::mock::serve(app).await;
    (base, state)
}

// ---------------------------------------------------------------------------
// Router under test
// ---------------------------------------------------------------------------

async fn build_app(cache: Arc<MemoryPromptCache>, mock_base: &str) -> Router {
    let hasher = KeyHasher::new(b"test-pepper".to_vec());
    let api_key_store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(hasher.clone()));
    api_key_store
        .insert_raw(ApiKey {
            key_hash: hasher.hash(TEST_API_KEY),
            key_prefix: openai_server::hashing::key_prefix(TEST_API_KEY),
            description: "anthropic prompt cache test key".to_string(),
            created_at: chrono::Utc::now(),
            permission: ApiKeyPermission {
                cache: Some(CachePolicy::new(CacheScope::Token)),
                ..ApiKeyPermission::default()
            },
            labels: HashMap::new(),
            usage_pool_id: None,
            daily_token_budget: None,
        })
        .await
        .expect("seed test key");

    let conns = Arc::new(InMemoryProviderConnectionStore::new());
    conns
        .create(ConnectionInput {
            name: "anthropic".to_string(),
            provider_kind: "anthropic".to_string(),
            base_url: Some(format!("{mock_base}/v1")),
            prefix: Some("anthropic/".to_string()),
            exposure: ExposurePolicy::Dynamic,
            routing_enabled: true,
            api_key: "sk-ant-test".to_string(),
            declared_capabilities: None,
        })
        .await
        .expect("seed anthropic connection");
    let routes = Arc::new(InMemoryModelRouteStore::new());

    let state = crate::support::app::AppStateBuilder::new(Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        conns.clone(),
        routes.clone(),
    )))
    .api_key_store(api_key_store)
    .provider_connection_store(conns)
    .model_route_store(routes)
    .prompt_cache(cache)
    .build()
    .await;

    Router::new()
        .route("/v1/messages", post(handlers::anthropic::proxy_messages))
        .route(
            "/v1/messages/count_tokens",
            post(handlers::anthropic::proxy_messages_count_tokens),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

fn request(uri: &str, body: &Value, extra: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method(http::Method::POST)
        .uri(uri)
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(
            http::header::AUTHORIZATION,
            format!("Bearer {TEST_API_KEY}"),
        );
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    builder
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap()
}

/// `(cache header, body)` for one call through the router.
async fn call(app: &Router, uri: &str, body: &Value) -> (Option<String>, String) {
    call_with(app, uri, body, &[]).await
}

/// [`call`], with extra request headers.
async fn call_with(
    app: &Router,
    uri: &str,
    body: &Value,
    extra: &[(&str, &str)],
) -> (Option<String>, String) {
    let res = app
        .clone()
        .oneshot(request(uri, body, extra))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let status = res
        .headers()
        .get("x-chalk-cache")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

/// Entries are written from a spawned task, so wait for the count rather than
/// sleeping a fixed amount.
async fn await_entries(cache: &Arc<MemoryPromptCache>, expected: usize) {
    for _ in 0..200 {
        if cache.len() >= expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "cache never reached {expected} entries (stuck at {})",
        cache.len()
    );
}

#[tokio::test]
async fn anthropic_messages_are_cached_streaming_and_non_streaming() {
    let (mock_base, mock) = start_mock().await;

    let cache = Arc::new(MemoryPromptCache::new(64, 8 << 20));
    let app = build_app(cache.clone(), &mock_base).await;

    let non_streaming = json!({
        "model": "claude-haiku-4-5",
        "messages": [{"role": "user", "content": "Say hi"}],
        "max_tokens": 32,
    });

    // --- Non-streaming: identical body is answered without a second call ----
    let (status, first) = call(&app, "/v1/messages", &non_streaming).await;
    assert_eq!(status.as_deref(), Some("miss"));
    assert!(first.contains("Hello from the mock!"));
    await_entries(&cache, 1).await;

    let (status, second) = call(&app, "/v1/messages", &non_streaming).await;
    assert_eq!(status.as_deref(), Some("hit"));
    assert_eq!(second, first, "the replayed bytes are the upstream's own");
    assert_eq!(
        mock.messages_calls.load(Ordering::SeqCst),
        1,
        "the hit must not reach the upstream"
    );

    // --- Streaming: a separate entry, replayed verbatim ---------------------
    let streaming = json!({
        "model": "claude-haiku-4-5",
        "messages": [{"role": "user", "content": "Say hi"}],
        "max_tokens": 32,
        "stream": true,
    });

    let (status, first_stream) = call(&app, "/v1/messages", &streaming).await;
    assert_eq!(
        status.as_deref(),
        Some("miss"),
        "streaming does not share the non-streaming entry on this surface"
    );
    assert!(first_stream.contains("message_stop"));
    assert_eq!(mock.messages_calls.load(Ordering::SeqCst), 2);
    await_entries(&cache, 2).await;

    let (status, second_stream) = call(&app, "/v1/messages", &streaming).await;
    assert_eq!(status.as_deref(), Some("hit"));
    assert_eq!(
        second_stream, first_stream,
        "the SSE stream is replayed byte for byte"
    );
    assert_eq!(
        mock.messages_calls.load(Ordering::SeqCst),
        2,
        "the streaming hit must not reach the upstream"
    );

    // --- A different prompt still misses ------------------------------------
    let other = json!({
        "model": "claude-haiku-4-5",
        "messages": [{"role": "user", "content": "Say something else"}],
        "max_tokens": 32,
    });
    let (status, _) = call(&app, "/v1/messages", &other).await;
    assert_eq!(status.as_deref(), Some("miss"));
    assert_eq!(mock.messages_calls.load(Ordering::SeqCst), 3);

    // --- A beta opt-in is part of the key, not just the body ----------------
    // `non_streaming` is already cached from the very first scenario. Re-asking
    // it under `anthropic-beta` is a *different* question — the response gains
    // an `input_transformations` array — so it must miss rather than replay the
    // opted-out recording, which would tell the caller that no thinking blocks
    // were dropped when some were.
    let beta = [("anthropic-beta", THINKING_BINDING_BETA)];
    let (status, opted_in) = call_with(&app, "/v1/messages", &non_streaming, &beta).await;
    assert_eq!(
        status.as_deref(),
        Some("miss"),
        "a beta opt-in must not hit the entry stored without it"
    );
    assert!(
        opted_in.contains("input_transformations"),
        "the opted-in caller gets the upstream's own opted-in response: {opted_in}"
    );
    assert_eq!(mock.messages_calls.load(Ordering::SeqCst), 4);
    await_entries(&cache, 4).await;

    // The opted-in ask now has its own entry...
    let (status, replayed) = call_with(&app, "/v1/messages", &non_streaming, &beta).await;
    assert_eq!(status.as_deref(), Some("hit"));
    assert_eq!(replayed, opted_in);
    assert_eq!(mock.messages_calls.load(Ordering::SeqCst), 4);

    // ...and the opted-out entry is untouched by it.
    let (status, plain) = call(&app, "/v1/messages", &non_streaming).await;
    assert_eq!(status.as_deref(), Some("hit"));
    assert_eq!(
        plain, first,
        "the opted-out caller still gets its own recording"
    );
    assert!(!plain.contains("input_transformations"));
    assert_eq!(mock.messages_calls.load(Ordering::SeqCst), 4);

    // Betas are a set: reordering and whitespace describe the same opt-in and
    // must hit, or a caller changing SDKs silently loses every entry.
    let reordered = [(
        "anthropic-beta",
        // A second beta, listed the other way round and loosely spaced.
        "compact-2026-01-12, thinking-binding-controls-2026-08-01",
    )];
    let (status, _) = call_with(&app, "/v1/messages", &non_streaming, &reordered).await;
    assert_eq!(
        status.as_deref(),
        Some("miss"),
        "a different beta set is a different key"
    );
    await_entries(&cache, 5).await;
    let (status, _) = call_with(
        &app,
        "/v1/messages",
        &non_streaming,
        &[(
            "anthropic-beta",
            "thinking-binding-controls-2026-08-01,compact-2026-01-12",
        )],
    )
    .await;
    assert_eq!(
        status.as_deref(),
        Some("hit"),
        "beta order does not shard the cache"
    );
    assert_eq!(mock.messages_calls.load(Ordering::SeqCst), 5);

    // --- count_tokens is never cached ---------------------------------------
    for _ in 0..2 {
        let (status, _) = call(&app, "/v1/messages/count_tokens", &non_streaming).await;
        assert_eq!(
            status, None,
            "token counting is not a generation and is not cached"
        );
    }
    assert_eq!(
        mock.count_tokens_calls.load(Ordering::SeqCst),
        2,
        "both count_tokens calls reached the upstream"
    );
}
