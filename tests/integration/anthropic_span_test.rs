//! OpenInference span coverage and native-body fidelity tests for the Anthropic
//! `/v1/messages` passthrough.
//!
//! Mirrors `tests/span_test.rs`'s otel harness (`InMemorySpanExporter` +
//! `SimpleSpanProcessor`) and `tests/anthropic_streaming_test.rs`'s mock-upstream
//! pattern (a local axum server standing in for `api.anthropic.com`), wired
//! through a real `RouterBackend` + `InMemoryProviderConnectionStore` holding a
//! stored, enabled `anthropic` connection (as in
//! `tests/anthropic_passthrough_test.rs`) so `resolve_anthropic` succeeds.
//!
//! The tests use a stored, test-only Anthropic connection and recording mock
//! gateway, matching production endpoint resolution without placing a real
//! credential on the network.

use std::collections::HashMap;
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
    provider_connections::{
        ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore, ProviderConnectionStore,
    },
};

use crate::support::app::AppStateBuilder;
use crate::support::otel::{setup_otel, span_attrs};

const TEST_API_KEY: &str = "test-key-for-anthropic-spans";

// ---------------------------------------------------------------------------
// Mock Anthropic `/v1/messages` — branches on the request's `stream` field so
// one server (and therefore one stored `base_url`) serves both scenarios.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct MockState {
    captured_body: Arc<tokio::sync::Mutex<Option<Value>>>,
    captured_api_key: Arc<tokio::sync::Mutex<Option<String>>>,
}

/// The scripted SSE event sequence: message_start -> content_block_start ->
/// two content_block_delta text_deltas -> message_delta (usage + stop_reason)
/// -> message_stop, each preceded by a ~75ms sleep. The first delta therefore
/// lands after 3 sleeps (~225ms, comfortably >= the 100ms floor asserted
/// below), and the full sequence takes ~450ms (comfortably >= the 300ms floor
/// asserted on the exported span's duration).
fn scripted_events() -> Vec<(&'static str, Value)> {
    vec![
        (
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_span_test",
                    "type": "message",
                    "role": "assistant",
                    "model": "claude-haiku-4-5",
                    "content": [],
                    "usage": {
                        "input_tokens": 12,
                        "output_tokens": 0,
                        "cache_read_input_tokens": 5,
                        "cache_creation_input_tokens": 1
                    }
                }
            }),
        ),
        (
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            }),
        ),
        (
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": "Hello"}
            }),
        ),
        (
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": ", world!"}
            }),
        ),
        (
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 7}
            }),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ]
}

async fn handle_messages(
    State(state): State<MockState>,
    headers: http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    *state.captured_body.lock().await = Some(body.clone());
    *state.captured_api_key.lock().await = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let streaming = body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    if streaming {
        let (tx, rx) =
            tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::convert::Infallible>>(32);
        tokio::spawn(async move {
            for (name, data) in scripted_events() {
                tokio::time::sleep(Duration::from_millis(75)).await;
                let payload = format!(
                    "event: {}\ndata: {}\n\n",
                    name,
                    serde_json::to_string(&data).unwrap()
                );
                if tx.send(Ok(bytes::Bytes::from(payload))).await.is_err() {
                    return;
                }
            }
        });
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(Body::from_stream(stream))
            .unwrap()
    } else {
        let model = body
            .get("model")
            .cloned()
            .unwrap_or(json!("claude-haiku-4-5"));
        let resp = json!({
            "id": "msg_span_test_ns",
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": [{"type": "text", "text": "Hello from the mock!"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 11,
                "output_tokens": 6,
                "cache_read_input_tokens": 3,
                "cache_creation_input_tokens": 2
            }
        });
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&resp).unwrap()))
            .unwrap()
    }
}

async fn start_mock() -> (String, MockState) {
    let state = MockState::default();
    let app = Router::new()
        .route("/v1/messages", post(handle_messages))
        .route("/v1/messages/count_tokens", post(handle_messages))
        .with_state(state.clone());
    let base_url = crate::support::mock::serve(app).await;
    (base_url, state)
}

// ---------------------------------------------------------------------------
// App wiring: a real `RouterBackend` over an `InMemoryProviderConnectionStore`
// holding a stored, enabled `anthropic` connection, so `resolve_anthropic`
// succeeds. The stored `base_url` is shared by the native passthrough and the
// translated Anthropic backend.
// ---------------------------------------------------------------------------

async fn build_app(mock_base: &str) -> Router {
    let hasher = KeyHasher::new(b"test-pepper".to_vec());
    let api_key_store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(hasher.clone()));
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

    let conns = Arc::new(InMemoryProviderConnectionStore::new());
    conns
        .create(ConnectionInput {
            name: "anthropic".to_string(),
            provider_kind: "anthropic".to_string(),
            base_url: Some(format!("{mock_base}/v1")),
            prefix: Some("anthropic/".to_string()),
            exposure: ExposurePolicy::Dynamic,
            routing_enabled: true,
            declared_capabilities: None,
            // A test-only backend: no real credential is ever sent to the
            // local mock upstream.
            api_key: "unused".to_string(),
        })
        .await
        .expect("seed anthropic connection");
    let routes = Arc::new(InMemoryModelRouteStore::new());

    let backend = Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        conns.clone(),
        routes.clone(),
    ));
    let state = AppStateBuilder::new(backend)
        .api_key_store(api_key_store)
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
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

fn post_messages_request(body: &Value) -> Request<Body> {
    Request::builder()
        .method(http::Method::POST)
        .uri("/v1/messages")
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(
            http::header::AUTHORIZATION,
            format!("Bearer {TEST_API_KEY}"),
        )
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap()
}

fn post_count_tokens_request(body: &Value) -> Request<Body> {
    Request::builder()
        .method(http::Method::POST)
        .uri("/v1/messages/count_tokens")
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(
            http::header::AUTHORIZATION,
            format!("Bearer {TEST_API_KEY}"),
        )
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn anthropic_count_tokens_strips_the_catalog_prefix() {
    let (mock_base, _) = start_mock().await;
    let app = build_app(&mock_base).await;
    let response = app
        .oneshot(post_count_tokens_request(&json!({
            "model": "anthropic/claude-haiku-4-5",
            "messages": [{"role": "user", "content": "Count this"}],
        })))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["model"], "claude-haiku-4-5");
}

#[tokio::test]
async fn anthropic_passthrough_only_rewrites_the_routed_model() {
    // This fixture deliberately bypasses the production connection validator:
    // it constructs an in-memory Anthropic connection with a dummy credential
    // and a loopback upstream, so native body fidelity can be tested without
    // ever allowing a real Anthropic connection to target localhost.
    // Serialize with the span fixture: the test process shares tracing state.
    let _guard = crate::support::otel::anthropic_base_lock();
    let (mock_base, mock) = start_mock().await;
    let app = build_app(&mock_base).await;
    let request_body = json!({
        "model": "anthropic/mock/t3",
        "system": [{
            "type": "text",
            "text": "You are a concise assistant.",
            "cache_control": {"type": "ephemeral"},
        }],
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "Describe this image."},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "abc"}},
                {"type": "tool_result", "tool_use_id": "tool-1", "content": "result"},
            ],
        }],
        "tools": [{
            "name": "lookup",
            "description": "Look something up",
            "input_schema": {"type": "object", "properties": {"query": {"type": "string"}}},
        }],
        "tool_choice": {"type": "auto"},
        "metadata": {"user_id": "test-user"},
        "max_tokens": 64,
        "temperature": 0.2,
        "future_option": {"nested": ["kept", true]},
        "stream": false,
    });

    let response = app
        .oneshot(post_messages_request(&request_body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let received = mock
        .captured_body
        .lock()
        .await
        .clone()
        .expect("mock upstream should receive the native request");
    let mut expected = request_body;
    expected["model"] = json!("mock/t3");
    assert_eq!(
        received, expected,
        "native Anthropic passthrough may rewrite only the routed model prefix"
    );
    assert_eq!(
        mock.captured_api_key.lock().await.as_deref(),
        Some("unused"),
        "the test fixture must never send a real Anthropic credential"
    );
}

#[tokio::test]
async fn anthropic_messages_span_records_full_attributes_for_non_streaming() {
    // Shared with `anthropic_prompt_cache_test`, which redirects the same
    // process-global variable (see `crate::support::otel::anthropic_base_lock`).
    let _guard = crate::support::otel::anthropic_base_lock();
    let (mock_base, _) = start_mock().await;

    let (provider, exporter, subscriber) = setup_otel();
    let app = build_app(&mock_base).await;

    let request_body = json!({
        "model": "anthropic/claude-haiku-4-5",
        "system": [{
            "type": "text",
            "text": "You are a terse assistant.",
            "cache_control": {"type": "ephemeral"},
        }],
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "Say hi"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "abc"}},
                {"type": "tool_result", "tool_use_id": "tool-1", "content": "result"},
            ],
        }],
        "tools": [{
            "name": "lookup",
            "description": "Look something up",
            "input_schema": {"type": "object"},
        }],
        "metadata": {"blob": "x".repeat(9 * 1024)},
        "max_tokens": 32,
        "stream": false,
    });

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let resp = app
            .oneshot(post_messages_request(&request_body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["content"][0]["text"], "Hello from the mock!");
        assert_eq!(
            body["model"], "claude-haiku-4-5",
            "the qualified catalog id must be stripped before native dispatch"
        );
    }

    for r in provider.force_flush() {
        r.expect("flush should succeed");
    }
    let spans = exporter.get_finished_spans().unwrap();
    let span = spans
        .iter()
        .find(|s| s.name.as_ref() == "anthropic.messages")
        .unwrap_or_else(|| {
            panic!(
                "expected an 'anthropic.messages' span; got: {:?}",
                spans.iter().map(|s| s.name.as_ref()).collect::<Vec<_>>()
            )
        });
    let attrs = span_attrs(span);

    assert_eq!(
        attrs.get("openinference.span.kind").map(String::as_str),
        Some("LLM")
    );
    assert_eq!(
        attrs.get("llm.system").map(String::as_str),
        Some("anthropic")
    );
    assert_eq!(
        attrs.get("llm.provider").map(String::as_str),
        Some("anthropic")
    );
    assert_eq!(
        attrs.get("llm.model_name").map(String::as_str),
        Some("claude-haiku-4-5")
    );
    assert_eq!(
        attrs
            .get("chalk.router.requested_model")
            .map(String::as_str),
        Some("anthropic/claude-haiku-4-5")
    );
    assert_eq!(attrs.get("llm.stream").map(String::as_str), Some("false"));
    assert!(
        attrs.contains_key("chalk.connection_id"),
        "attrs: {attrs:?}"
    );

    // `input_tokens` (11) EXCLUDES both cache fields — the true prompt
    // total sums input + cache_read (3) + cache_creation (2) = 16.
    assert_eq!(
        attrs.get("llm.token_count.prompt").map(String::as_str),
        Some("16"),
        "attrs: {attrs:?}"
    );
    assert_eq!(
        attrs.get("llm.token_count.completion").map(String::as_str),
        Some("6"),
        "attrs: {attrs:?}"
    );
    assert_eq!(
        attrs.get("llm.token_count.total").map(String::as_str),
        Some("22"),
        "prompt (16) + completion (6); attrs: {attrs:?}"
    );
    assert_eq!(
        attrs
            .get("llm.token_count.prompt_details.cache_read")
            .map(String::as_str),
        Some("3"),
        "attrs: {attrs:?}"
    );
    assert_eq!(
        attrs
            .get("llm.token_count.prompt_details.cache_write")
            .map(String::as_str),
        Some("2"),
        "attrs: {attrs:?}"
    );

    assert_eq!(
        attrs
            .get("llm.output_messages.0.message.content")
            .map(String::as_str),
        Some("Hello from the mock!"),
        "attrs: {attrs:?}"
    );

    // Input messages: system prompt hoisted to index 0, then the user turn.
    assert_eq!(
        attrs
            .get("llm.input_messages.0.message.role")
            .map(String::as_str),
        Some("system"),
        "attrs: {attrs:?}"
    );
    assert_eq!(
        attrs
            .get("llm.input_messages.0.message.content")
            .map(String::as_str),
        Some("You are a terse assistant."),
        "attrs: {attrs:?}"
    );
    assert_eq!(
        attrs
            .get("llm.input_messages.1.message.role")
            .map(String::as_str),
        Some("user"),
        "attrs: {attrs:?}"
    );
    assert_eq!(
        attrs
            .get("llm.input_messages.1.message.content")
            .map(String::as_str),
        Some("Say hi"),
        "attrs: {attrs:?}"
    );
    let raw_input: Value = serde_json::from_str(attrs.get("input.value").unwrap())
        .expect("input.value should preserve the Anthropic request");
    assert_eq!(raw_input["model"], "anthropic/claude-haiku-4-5");
    assert_eq!(raw_input["system"][0]["text"], "You are a terse assistant.");
    assert_eq!(raw_input["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(raw_input["messages"][0]["role"], "user");
    assert_eq!(raw_input["messages"][0]["content"][1]["type"], "image");
    assert_eq!(
        raw_input["messages"][0]["content"][2]["type"],
        "tool_result"
    );
    assert_eq!(raw_input["tools"][0]["name"], "lookup");
    assert_eq!(
        raw_input["metadata"]["blob"].as_str().map(str::len),
        Some(9 * 1024)
    );
    assert_eq!(raw_input["max_tokens"], 32);
    let invocation_parameters: Value = serde_json::from_str(
        attrs
            .get("llm.invocation_parameters")
            .expect("bounded standard invocation parameters should survive"),
    )
    .unwrap();
    assert_eq!(invocation_parameters["max_tokens"], 32);
    assert!(!invocation_parameters
        .as_object()
        .unwrap()
        .contains_key("metadata"));
    assert!(
        !attrs.contains_key("chalk.trace.input.truncation"),
        "unknown metadata is intentionally excluded, not budget-truncated: {attrs:?}"
    );
}

#[tokio::test]
async fn anthropic_messages_span_covers_the_full_sse_stream_for_streaming() {
    // The span must cover the whole SSE stream, not just the time to open it;
    // see the duration/ttft assertions below.
    let _guard = crate::support::otel::anthropic_base_lock();
    let (mock_base, _) = start_mock().await;

    let (provider, exporter, subscriber) = setup_otel();
    let app = build_app(&mock_base).await;

    let request_body = json!({
        "model": "anthropic/claude-haiku-4-5",
        "messages": [{"role": "user", "content": "Say hi"}],
        "max_tokens": 32,
        "stream": true,
    });

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let resp = app
            .oneshot(post_messages_request(&request_body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body_text = String::from_utf8_lossy(&body_bytes);
        assert!(
            body_text.contains("message_stop"),
            "body should contain message_stop: {body_text}"
        );
    }

    for r in provider.force_flush() {
        r.expect("flush should succeed");
    }
    let spans = exporter.get_finished_spans().unwrap();
    let span = spans
        .iter()
        .find(|s| s.name.as_ref() == "anthropic.messages")
        .unwrap_or_else(|| {
            panic!(
                "expected an 'anthropic.messages' span; got: {:?}",
                spans.iter().map(|s| s.name.as_ref()).collect::<Vec<_>>()
            )
        });
    let attrs = span_attrs(span);

    assert_eq!(attrs.get("llm.stream").map(String::as_str), Some("true"));

    // The span must cover the whole SSE stream (~450ms), not just the time
    // to open it — proof that `AnthropicSseSpanTap` re-enters the span on
    // every poll (see its doc comment for the mechanism).
    let duration = span
        .end_time
        .duration_since(span.start_time)
        .unwrap_or_default();
    assert!(
        duration >= Duration::from_millis(300),
        "anthropic.messages span should span the whole SSE stream (~450ms), got {duration:?}"
    );

    // First delta lands after 3 scripted sleeps (~225ms); the role-only /
    // block-start events must not have stamped this early.
    let ttft_ms: i64 = attrs
        .get("llm.time_to_first_token_ms")
        .unwrap_or_else(|| panic!("missing llm.time_to_first_token_ms; attrs: {attrs:?}"))
        .parse()
        .expect("llm.time_to_first_token_ms should be an integer");
    assert!(
        ttft_ms >= 100,
        "expected time-to-first-token >= 100ms, got {ttft_ms}"
    );

    assert_eq!(
        attrs
            .get("llm.output_messages.0.message.role")
            .map(String::as_str),
        Some("assistant"),
        "attrs: {attrs:?}"
    );
    assert_eq!(
        attrs
            .get("llm.output_messages.0.message.content")
            .map(String::as_str),
        Some("Hello, world!"),
        "assembled streamed content should equal the concatenated deltas; attrs: {attrs:?}"
    );

    // message_start reports input_tokens=12, cache_read=5, cache_creation=1
    // (input_tokens itself EXCLUDES both cache fields) -> prompt = 18.
    assert_eq!(
        attrs.get("llm.token_count.prompt").map(String::as_str),
        Some("18"),
        "attrs: {attrs:?}"
    );
    assert_eq!(
        attrs.get("llm.token_count.completion").map(String::as_str),
        Some("7"),
        "attrs: {attrs:?}"
    );
    assert_eq!(
        attrs.get("llm.token_count.total").map(String::as_str),
        Some("25"),
        "prompt (18) + completion (7); attrs: {attrs:?}"
    );
    assert_eq!(
        attrs
            .get("llm.token_count.prompt_details.cache_read")
            .map(String::as_str),
        Some("5"),
        "attrs: {attrs:?}"
    );
    assert_eq!(
        attrs
            .get("llm.token_count.prompt_details.cache_write")
            .map(String::as_str),
        Some("1"),
        "attrs: {attrs:?}"
    );
}
