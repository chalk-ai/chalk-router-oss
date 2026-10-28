//! Anthropic `/v1/messages` fallback: the primary connection returns a 5xx and
//! a configured fallback serves the request instead, for both non-streaming
//! and streaming clients.
//!
//! Modelled on `streaming_fallback_test.rs`'s mock-backend approach and
//! `anthropic_passthrough_test.rs`'s route wiring. `MockBackend` resolves
//! every route through the `openai-compatible` capability profile, which
//! gives Anthropic Messages `DeliveryMode::Unknown` — dispatched through
//! `adapters::anthropic_chat::translate_messages` -> `create_chat_completion`.
//! That path can fail with `Err(BackendError)` on a bad upstream status.
//!
//! `NativeMockBackend` instead resolves through the `anthropic` capability
//! profile, which gives `DeliveryMode::Native` — dispatched through
//! `send_anthropic_request`, returning a raw `reqwest::Response` rather than
//! an `Err`. A retryable upstream status (429 or 5xx) is converted to an
//! `Err` by `handle_raw_upstream_dispatch` so it drives the same fallback search as
//! the translated path, while every other non-2xx status (e.g. 400) is
//! forwarded to the client unchanged with no fallback attempted at all.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::{middleware, routing::post, Router};

use openai_server::{
    api_keys::{ApiKey, ApiKeyPermission, ApiKeyStore, InMemoryApiKeyStore},
    backend::{
        AnthropicRequestEndpoint, AnthropicRequestHeaders, BackendError, BackendProvider,
        ResolvedRoute, RouteResolution,
    },
    fallback::FallbackPolicy,
    hashing::KeyHasher,
    middleware::auth_middleware,
    types::{
        ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatMessage,
        CreateEmbeddingRequest, CreateEmbeddingResponse, CreateImageRequest,
        CreateModerationRequest, CreateModerationResponse, CreateSpeechRequest, ImageResponse,
        ListResponse, Model, TranscriptionResponse, Usage,
    },
    AppState,
};

const TEST_KEY: &str = "sk-anthropic-messages-fallback-test";

/// Mock backend that records every dispatched `create_chat_completion` call so
/// a test can assert exactly which candidates were tried, in order. A model
/// name containing "fail" fails dispatch with a 502, mirroring
/// `streaming_fallback_test.rs`'s naming convention.
#[derive(Clone, Default)]
struct MockBackend {
    chat_calls: Arc<Mutex<Vec<String>>>,
}

impl MockBackend {
    fn dispatched(&self) -> Vec<String> {
        self.chat_calls.lock().unwrap().clone()
    }

    fn failure_for(model: &str) -> Option<BackendError> {
        model
            .contains("fail")
            .then(|| BackendError::api_status(502, "upstream failed"))
    }

    /// Every route resolves through the `openai-compatible` capability
    /// profile, which gives Anthropic Messages `DeliveryMode::Unknown` (a
    /// mockable, `Err`-capable dispatch path) rather than the real Anthropic
    /// provider's `Native` mode (which never retries on an upstream 5xx).
    fn resolved_route(&self, model: &str) -> ResolvedRoute {
        ResolvedRoute {
            routing: Default::default(),
            backend: Arc::new(self.clone()),
            stripped_model: model.to_string(),
            capabilities: Arc::new(
                openai_server::capabilities::resolve("openai-compatible", model, None, None)
                    .unwrap(),
            ),
            resolution: RouteResolution {
                provider_kind: "openai-compatible".to_string(),
                connection_id: Some("openai-compatible".to_string()),
                connection_name: "openai-compatible".to_string(),
                upstream_model_id: model.to_string(),
                connection_prefix: None,
                model_route_id: None,
            },
        }
    }
}

/// A fixed chat-completion response naming the model it was dispatched to, so
/// a test can prove the body came from the winning fallback.
fn fixed_chat_response(model: &str) -> ChatCompletionResponse {
    ChatCompletionResponse {
        id: "chatcmpl-amf".to_string(),
        object: "chat.completion".to_string(),
        created: 1,
        model: model.to_string(),
        choices: vec![ChatChoice {
            index: 0,
            message: ChatMessage {
                role: "assistant".to_string(),
                content: Some(serde_json::Value::String("ok".to_string())),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            },
            finish_reason: Some("stop".to_string()),
        }],
        usage: Some(Usage {
            prompt_tokens: 1,
            completion_tokens: Some(1),
            total_tokens: 2,
            ..Default::default()
        }),
    }
}

#[async_trait]
impl BackendProvider for MockBackend {
    async fn create_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        self.chat_calls.lock().unwrap().push(request.model.clone());
        match Self::failure_for(&request.model) {
            Some(e) => Err(e),
            None => Ok(fixed_chat_response(&request.model)),
        }
    }

    async fn create_embedding(
        &self,
        _r: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_image(&self, _r: CreateImageRequest) -> Result<ImageResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_speech(&self, _r: CreateSpeechRequest) -> Result<bytes::Bytes, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_transcription(
        &self,
        _model: String,
        _b: bytes::Bytes,
        _f: String,
        _l: Option<String>,
        _p: Option<String>,
        _fmt: Option<String>,
        _t: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_translation(
        &self,
        _model: String,
        _b: bytes::Bytes,
        _f: String,
        _p: Option<String>,
        _fmt: Option<String>,
        _t: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_moderation(
        &self,
        _r: CreateModerationRequest,
    ) -> Result<CreateModerationResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn list_models(&self) -> Result<ListResponse<Model>, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn get_model(&self, _id: &str) -> Result<Model, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    fn name(&self) -> &str {
        "mock"
    }

    /// Used for the primary route.
    async fn resolve_anthropic(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        Ok(self.resolved_route(model))
    }

    /// Used for fallback candidates.
    async fn resolve_strict(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        Ok(self.resolved_route(model))
    }
}

/// Mock backend that resolves through the `anthropic` capability profile, so
/// Anthropic Messages dispatches through `DeliveryMode::Native` ->
/// `send_anthropic_request` — a raw `reqwest::Response`, not an `Err` — the
/// path this file's native-fallback tests exercise. A model name containing
/// "fail-5xx" or "fail-400" returns that status with a canned Anthropic-style
/// error body instead of a success body.
#[derive(Clone, Default)]
struct NativeMockBackend {
    calls: Arc<Mutex<Vec<String>>>,
}

impl NativeMockBackend {
    fn dispatched(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn resolved_route(&self, model: &str) -> ResolvedRoute {
        ResolvedRoute {
            routing: Default::default(),
            backend: Arc::new(self.clone()),
            stripped_model: model.to_string(),
            capabilities: Arc::new(
                openai_server::capabilities::resolve("anthropic", model, None, None).unwrap(),
            ),
            resolution: RouteResolution {
                provider_kind: "anthropic".to_string(),
                connection_id: Some("anthropic".to_string()),
                connection_name: "anthropic".to_string(),
                upstream_model_id: model.to_string(),
                connection_prefix: None,
                model_route_id: None,
            },
        }
    }

    fn response_for(model: &str, streaming: bool) -> reqwest::Response {
        let (status, content_type, body): (u16, &str, Vec<u8>) = if model.contains("fail-5xx") {
            (
                503,
                "application/json",
                br#"{"type":"error","error":{"type":"overloaded_error","message":"upstream overloaded"}}"#
                    .to_vec(),
            )
        } else if model.contains("fail-400") {
            (
                400,
                "application/json",
                br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad request"}}"#
                    .to_vec(),
            )
        } else if streaming {
            (200, "text/event-stream", native_sse_body(model))
        } else {
            (200, "application/json", native_json_body(model))
        };
        let http_resp = http::Response::builder()
            .status(status)
            .header("content-type", content_type)
            .body(body)
            .unwrap();
        http_resp.into()
    }
}

/// A fixed native Anthropic Messages JSON response naming the model it was
/// dispatched to, so a test can prove the body came from the winning
/// fallback.
fn native_json_body(model: &str) -> Vec<u8> {
    let input_tokens = native_input_tokens(model);
    serde_json::json!({
        "id": "msg_native_test",
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": input_tokens, "output_tokens": 2},
    })
    .to_string()
    .into_bytes()
}

/// A model name containing "hosted-usage" reports the input a provider-run
/// server tool adds, far beyond the visible request.
fn native_input_tokens(model: &str) -> u64 {
    if model.contains("hosted-usage") {
        8_000
    } else {
        5
    }
}

/// A fixed native Anthropic Messages SSE stream naming the model it was
/// dispatched to, using the real Anthropic Messages streaming protocol (not
/// the router's translated-from-chat-completions shape used by
/// `MockBackend`): `message_start` -> `content_block_start` ->
/// `content_block_delta` (text) -> `content_block_stop` -> `message_delta`
/// (usage + stop_reason) -> `message_stop`.
fn native_sse_body(model: &str) -> Vec<u8> {
    let mut out = String::new();
    let mut push = |event: &str, data: serde_json::Value| {
        out.push_str(&format!("event: {event}\ndata: {data}\n\n"));
    };
    push(
        "message_start",
        serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg_native_test",
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": native_input_tokens(model), "output_tokens": 0},
            }
        }),
    );
    push(
        "content_block_start",
        serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""},
        }),
    );
    push(
        "content_block_delta",
        serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "ok"},
        }),
    );
    push(
        "content_block_stop",
        serde_json::json!({"type": "content_block_stop", "index": 0}),
    );
    push(
        "message_delta",
        serde_json::json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": {"output_tokens": 2},
        }),
    );
    push("message_stop", serde_json::json!({"type": "message_stop"}));
    out.into_bytes()
}

fn request_model(body: &bytes::Bytes) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("model")
                .and_then(|m| m.as_str().map(str::to_string))
        })
        .unwrap_or_default()
}

#[async_trait]
impl BackendProvider for NativeMockBackend {
    async fn create_chat_completion(
        &self,
        _r: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_embedding(
        &self,
        _r: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_image(&self, _r: CreateImageRequest) -> Result<ImageResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_speech(&self, _r: CreateSpeechRequest) -> Result<bytes::Bytes, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_transcription(
        &self,
        _model: String,
        _b: bytes::Bytes,
        _f: String,
        _l: Option<String>,
        _p: Option<String>,
        _fmt: Option<String>,
        _t: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_translation(
        &self,
        _model: String,
        _b: bytes::Bytes,
        _f: String,
        _p: Option<String>,
        _fmt: Option<String>,
        _t: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_moderation(
        &self,
        _r: CreateModerationRequest,
    ) -> Result<CreateModerationResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn list_models(&self) -> Result<ListResponse<Model>, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn get_model(&self, _id: &str) -> Result<Model, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    fn name(&self) -> &str {
        "mock-native"
    }

    async fn send_anthropic_request(
        &self,
        _endpoint: AnthropicRequestEndpoint,
        _headers: &AnthropicRequestHeaders,
        body: bytes::Bytes,
    ) -> Result<reqwest::Response, BackendError> {
        let model = request_model(&body);
        let streaming = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
            .unwrap_or(false);
        self.calls.lock().unwrap().push(model.clone());
        Ok(Self::response_for(&model, streaming))
    }

    /// Used for the primary route.
    async fn resolve_anthropic(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        Ok(self.resolved_route(model))
    }

    /// Used for fallback candidates.
    async fn resolve_strict(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        Ok(self.resolved_route(model))
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Setup {
    backend: Arc<MockBackend>,
    port: u16,
}

async fn state_with<B: BackendProvider + 'static>(
    backend: Arc<B>,
    api_key_store: Arc<dyn ApiKeyStore>,
    fallbacks: FallbackPolicy,
) -> AppState {
    crate::support::app::AppStateBuilder::new(backend)
        .api_key_store(api_key_store)
        .fallback_policy(fallbacks)
        .build()
        .await
}

async fn key_store(permission: ApiKeyPermission) -> Arc<dyn ApiKeyStore> {
    let hasher = KeyHasher::new(b"test-pepper".to_vec());
    let store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(hasher.clone()));
    store
        .insert_raw(ApiKey {
            key_hash: hasher.hash(TEST_KEY),
            key_prefix: openai_server::hashing::key_prefix(TEST_KEY),
            description: "anthropic messages fallback test key".to_string(),
            created_at: chrono::Utc::now(),
            permission,
            labels: HashMap::new(),
            usage_pool_id: None,
            daily_token_budget: None,
        })
        .await
        .expect("seed key");
    store
}

fn app(state: AppState) -> Router {
    Router::new()
        .route(
            "/v1/messages",
            post(openai_server::handlers::anthropic::proxy_messages),
        )
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, auth_middleware))
}

async fn boot(fallbacks: FallbackPolicy) -> Setup {
    let backend = Arc::new(MockBackend::default());
    let store = key_store(ApiKeyPermission::default()).await;
    let state = state_with(backend.clone(), store, fallbacks).await;
    let app = app(state);
    let base = crate::support::mock::serve(app).await;
    let port = base.rsplit(':').next().unwrap().parse().unwrap();
    Setup { backend, port }
}

struct NativeSetup {
    backend: Arc<NativeMockBackend>,
    port: u16,
}

async fn boot_native(fallbacks: FallbackPolicy) -> NativeSetup {
    let backend = Arc::new(NativeMockBackend::default());
    let store = key_store(ApiKeyPermission::default()).await;
    let state = state_with(backend.clone(), store, fallbacks).await;
    let app = app(state);
    let base = crate::support::mock::serve(app).await;
    let port = base.rsplit(':').next().unwrap().parse().unwrap();
    NativeSetup { backend, port }
}

fn policy(primary: &str, alternates: &[&str]) -> FallbackPolicy {
    let mut map = HashMap::new();
    map.insert(
        primary.to_string(),
        alternates.iter().map(|s| s.to_string()).collect(),
    );
    FallbackPolicy::from_map(map)
}

async fn post_messages(port: u16, model: &str, streaming: bool) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .bearer_auth(TEST_KEY)
        .json(&serde_json::json!({
            "model": model,
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hi"}],
            "stream": streaming,
        }))
        .send()
        .await
        .expect("send")
}

fn header(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn primary_5xx_falls_back_to_serve_non_streaming() {
    // The primary connection returns a 5xx; the configured fallback serves the
    // request and the client sees a clean 200.
    let setup = boot(policy("anthropic/fail-primary", &["anthropic/good-alt"])).await;

    let resp = post_messages(setup.port, "anthropic/fail-primary", false).await;

    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(
        header(&resp, "x-fallback-model").as_deref(),
        Some("anthropic/good-alt")
    );
    // The routed-model header names what ACTUALLY served, not what was
    // requested.
    assert_eq!(
        header(&resp, "x-chalk-routed-model").as_deref(),
        Some("anthropic/good-alt")
    );

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "message");
    assert_eq!(body["model"], "anthropic/good-alt");
    assert_eq!(body["content"][0]["text"], "ok");

    assert_eq!(
        setup.backend.dispatched(),
        vec!["anthropic/fail-primary", "anthropic/good-alt"]
    );
}

#[tokio::test]
async fn primary_5xx_falls_back_to_serve_streaming() {
    // Same as above, but for a streaming client: the fallback's SSE reaches
    // the client and nothing from the failed primary leaks into it.
    let setup = boot(policy("anthropic/fail-primary", &["anthropic/good-alt"])).await;

    let resp = post_messages(setup.port, "anthropic/fail-primary", true).await;

    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "content-type").as_deref(),
        Some("text/event-stream")
    );
    assert_eq!(
        header(&resp, "x-fallback-model").as_deref(),
        Some("anthropic/good-alt")
    );
    assert_eq!(
        header(&resp, "x-chalk-routed-model").as_deref(),
        Some("anthropic/good-alt")
    );

    let body = resp.text().await.unwrap();
    assert!(
        body.contains("anthropic/good-alt"),
        "SSE body must come from the winning fallback; got: {body}"
    );
    assert!(
        body.contains("\"text\":\"ok\""),
        "SSE body must carry the fallback's content; got: {body}"
    );
    assert!(
        !body.contains("anthropic/fail-primary"),
        "no bytes may leak from the failed primary; got: {body}"
    );

    assert_eq!(
        setup.backend.dispatched(),
        vec!["anthropic/fail-primary", "anthropic/good-alt"]
    );
}

// ---------------------------------------------------------------------------
// Native/adapted transport: retryable statuses (429/5xx) drive the same
// fallback search as the translated path; other non-2xx statuses (e.g. 400)
// pass straight through with no fallback attempted.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn native_5xx_falls_back_to_serve_non_streaming() {
    // A native upstream 503 is retryable: the configured fallback serves the
    // request and the client sees a clean 200, exactly as the translated
    // path does.
    let setup = boot_native(policy(
        "anthropic/fail-5xx-primary",
        &["anthropic/native-good-alt"],
    ))
    .await;

    let resp = post_messages(setup.port, "anthropic/fail-5xx-primary", false).await;

    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(
        header(&resp, "x-fallback-model").as_deref(),
        Some("anthropic/native-good-alt")
    );
    assert_eq!(
        header(&resp, "x-chalk-routed-model").as_deref(),
        Some("anthropic/native-good-alt")
    );

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "message");
    assert_eq!(body["model"], "anthropic/native-good-alt");
    assert_eq!(body["content"][0]["text"], "ok");

    assert_eq!(
        setup.backend.dispatched(),
        vec!["anthropic/fail-5xx-primary", "anthropic/native-good-alt"]
    );
}

#[tokio::test]
async fn native_5xx_falls_back_to_serve_streaming() {
    // Same as above, but for a streaming client: the fallback's native SSE
    // stream reaches the client byte-for-byte and nothing from the failed
    // primary leaks into it.
    let setup = boot_native(policy(
        "anthropic/fail-5xx-primary",
        &["anthropic/native-good-alt"],
    ))
    .await;

    let resp = post_messages(setup.port, "anthropic/fail-5xx-primary", true).await;

    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "content-type").as_deref(),
        Some("text/event-stream")
    );
    assert_eq!(
        header(&resp, "x-fallback-model").as_deref(),
        Some("anthropic/native-good-alt")
    );
    assert_eq!(
        header(&resp, "x-chalk-routed-model").as_deref(),
        Some("anthropic/native-good-alt")
    );

    let body = resp.text().await.unwrap();
    assert!(
        body.contains("anthropic/native-good-alt"),
        "SSE body must come from the winning fallback; got: {body}"
    );
    assert!(
        body.contains("\"text\":\"ok\""),
        "SSE body must carry the fallback's content; got: {body}"
    );
    assert!(
        body.contains("message_stop"),
        "the real Anthropic message_stop event must reach the client verbatim; got: {body}"
    );
    assert!(
        !body.contains("anthropic/fail-5xx-primary"),
        "no bytes may leak from the failed primary; got: {body}"
    );

    assert_eq!(
        setup.backend.dispatched(),
        vec!["anthropic/fail-5xx-primary", "anthropic/native-good-alt"]
    );
}

#[tokio::test]
async fn native_5xx_without_fallback_passes_through_upstream_bytes() {
    // With no alternates configured, exhausting the (single) candidate must
    // forward the original upstream status/body byte-for-byte via
    // `raw_upstream_failure_response`, not the router's own error envelope.
    let setup = boot_native(policy("anthropic/fail-5xx-primary", &[])).await;

    let resp = post_messages(setup.port, "anthropic/fail-5xx-primary", false).await;

    assert_eq!(resp.status(), 503);
    assert_eq!(
        header(&resp, "content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(header(&resp, "x-fallback-model"), None);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "overloaded_error");
    assert_eq!(body["error"]["message"], "upstream overloaded");

    assert_eq!(
        setup.backend.dispatched(),
        vec!["anthropic/fail-5xx-primary"]
    );
}

#[tokio::test]
async fn native_400_never_retries_even_with_fallback_configured() {
    // A native upstream 400 is not retryable: it must reach the client
    // unchanged and the configured fallback must never be dispatched, even
    // though one is available.
    let setup = boot_native(policy(
        "anthropic/fail-400-primary",
        &["anthropic/native-good-alt"],
    ))
    .await;

    let resp = post_messages(setup.port, "anthropic/fail-400-primary", false).await;

    assert_eq!(resp.status(), 400);
    assert_eq!(
        header(&resp, "content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(header(&resp, "x-fallback-model"), None);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["message"], "bad request");

    assert_eq!(
        setup.backend.dispatched(),
        vec!["anthropic/fail-400-primary"]
    );
}

#[tokio::test]
async fn provider_input_admits_under_a_budget_and_settles_reported_usage_as_debt() {
    use openai_server::budget::{BudgetPolicy, MemoryBudgetPolicyStore};

    let model = "claude-hosted-usage";
    // Server tools and opaque content (images, documents) both add input the
    // router cannot count, so both are soft caps.
    let server_tool = serde_json::json!({
        "messages": [{"role": "user", "content": "What changed today?"}],
        "tools": [{"type": "web_search_20250305", "name": "web_search"}],
    });
    let image = serde_json::json!({
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "Describe this"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                "data": "A".repeat(4_096)}},
            {"type": "document", "source": {"type": "base64",
                "media_type": "application/pdf", "data": "JVBERi0="}},
        ]}],
    });
    for (shape, streaming) in [
        (&server_tool, false),
        (&server_tool, true),
        (&image, false),
        (&image, true),
    ] {
        let backend = Arc::new(NativeMockBackend::default());
        let budgets = Arc::new(MemoryBudgetPolicyStore::new("test".into()));
        budgets
            .insert(BudgetPolicy {
                quota_policy_id: "messages-token-budget".into(),
                id: "messages-token-budget-source".into(),
                environment_id: "test".into(),
                scope: "api_key".into(),
                scope_id: KeyHasher::new(b"test-pepper".to_vec()).hash(TEST_KEY),
                period: "daily".into(),
                window_type: "calendar".into(),
                window_size: sqlx::postgres::types::PgInterval {
                    months: 0,
                    days: 1,
                    microseconds: 0,
                },
                anchor: None,
                timezone: "UTC".into(),
                limit_kind: "tokens".into(),
                limit_amount: 512,
                enabled: true,
                created_at: chrono::Utc::now(),
            })
            .unwrap();
        let rates = Arc::new(openai_server::ratelimit::MemoryRateLimitPolicyStore::new(
            "test".into(),
        ));
        let state = crate::support::app::AppStateBuilder::new(backend.clone())
            .api_key_store(key_store(ApiKeyPermission::default()).await)
            .quota_resolver(Arc::new(
                openai_server::quota_policy::QuotaPolicyResolver::new(
                    "test".into(),
                    budgets.clone(),
                    rates.clone(),
                ),
            ))
            .budget_store(budgets)
            .rate_limit_store(rates)
            .quota_backend(Arc::new(openai_server::quota::MemoryQuota::new()))
            .build()
            .await;
        let base = crate::support::mock::serve(app(state)).await;
        let send = || {
            reqwest::Client::new()
                .post(format!("{base}/v1/messages"))
                .bearer_auth(TEST_KEY)
                .json(&{
                    let mut body = shape.clone();
                    body["model"] = model.into();
                    body["max_tokens"] = 64.into();
                    body["stream"] = streaming.into();
                    body
                })
                .send()
        };

        let admitted = send().await.unwrap();
        assert_eq!(admitted.status(), 200, "stream={streaming} {shape}");
        assert!(admitted.text().await.unwrap().contains("8000"));
        // The reported 8,002 tokens settle as debt against the 512 budget.
        let rejected = send().await.unwrap();
        assert_eq!(rejected.status(), 429, "stream={streaming} {shape}");
        assert_eq!(backend.dispatched(), vec![model.to_string()]);
    }
}
