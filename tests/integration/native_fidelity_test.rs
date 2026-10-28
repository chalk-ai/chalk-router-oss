//! Literal wire fixtures through the real handler, router, and HTTP adapter.
//! Raw strings deliberately include fields absent from the observation types.
use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    middleware,
    response::Response,
    routing::{get, post},
    Router,
};
use openai_server::{
    api_keys::{
        ApiKey, ApiKeyPermission, ApiKeyStore, ApiKeyStoreError, CreatedApiKey, InMemoryApiKeyStore,
    },
    backend::{BackendProvider, RouterBackend},
    fallback::FallbackPolicy,
    handlers,
    hashing::KeyHasher,
    middleware::auth_middleware,
    model_routes::{InMemoryModelRouteStore, ModelRouteStore, RouteInput},
    native_chat::NativeChatRequest,
    prompt_cache::{memory::MemoryPromptCache, CachePolicy, CacheScope},
    provider_connections::{
        ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore, ProviderConnectionStore,
    },
    service::pagination::StablePageCursor,
    types::{CreateEmbeddingRequest, EmbeddingInput},
    AppState,
};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tower::ServiceExt;

use super::support::{app::AppStateBuilder, mock::serve};

const RICH_REPLY: &str = r#"{ "id":"chatcmpl-native", "object":"chat.completion", "created":1, "model":"gpt-4o", "choices":[{"index":0,"message":{"role":"assistant","content":null,"refusal":"Cannot help","annotations":[{"type":"url_citation","url":"https://example.com","future":null}]},"finish_reason":"stop","logprobs":{"content":[{"token":"x","logprob":-0.1234567890123456789}]}}], "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10,"prompt_tokens_details":{"cached_tokens":0,"audio_tokens":2,"future":null},"completion_tokens_details":{"reasoning_tokens":1,"audio_tokens":0,"accepted_prediction_tokens":2,"rejected_prediction_tokens":0}}, "system_fingerprint":"fp_native", "service_tier":"priority", "future":{"decimal":1.0000000000000001,"explicit_null":null} }"#;
const RESPONSES_REPLY: &str = r#"  { "id":"resp-native", "object":"response", "status":"completed", "model":"gpt-4o", "output":[{"id":"msg-native","type":"message","role":"assistant","content":[{"type":"output_text","text":"hello","annotations":[]}]}], "usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}, "future":{"z":1.0000000000000001,"a":null,"duplicate":1,"duplicate":2}, "escaped":"\u0061" }  "#;
const DELETED_REPLY: &str = r#" {"id":"resp-native","object":"response.deleted","deleted":true,"future":1.0000000000000001} "#;
fn responses_sse() -> String {
    format!(
        ": keepalive\r\n\r\nid: event-0\r\nretry: 321\r\nx-provider: retained\r\nevent: response.created\r\ndata: {{ \"type\":\"response.created\", \"response\":{} }}\r\n\r\nevent: response.output_text.delta\ndata: {{ \"type\":\"response.output_text.delta\", \"response_id\":\"resp-native\",\n: between data lines\ndata: \"delta\":\"hé🦀\", \"future\":1.0000000000000001 }}\n\nevent: response.completed\ndata: {{ \"type\":\"response.completed\", \"response\":{} }}\n\n: trailing comment without terminator",
        RESPONSES_REPLY.replace("completed", "in_progress"),
        RESPONSES_REPLY
    )
}
const EMBEDDING_REPLY: &str = r#"{ "object":"list", "model":"text-embedding-3-small", "data":[{"object":"embedding","index":0,"embedding":[0.1234567890123456789,-0.000000000000000001],"future":null}], "usage":{"prompt_tokens":3,"total_tokens":3,"future":0}, "future":{"decimal":1.0000000000000001} }"#;
const IMAGE_REPLY: &str = r#"{ "created":17, "data":[{"b64_json":"aW1hZ2U=","revised_prompt":"hello","future":null}], "usage":{"input_tokens":3,"output_tokens":5,"input_tokens_details":{"image_tokens":2}}, "future":1.0000000000000001 }"#;
const SSE_REPLY: &str = concat!(
    ": provider-comment\n\nid: native-event\ndata: {\"id\":\"chatcmpl-native\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":null,\"refusal\":\"Cannot help\"},\"finish_reason\":null}],\"service_tier\":\"priority\"}\n\n",
    "data: {\"id\":\"chatcmpl-native\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    ": before-usage\n\n",
    "data: {\"id\":\"chatcmpl-native\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"total_tokens\":10}}\n\n",
    "data: [DONE]\n\n"
);

/// The shape OpenAI itself sends: informational metadata plus explicit nulls
/// that carry nothing a cache replay would lose.
const PLAIN_REPLY: &str = r#"{"id":"chatcmpl-plain","object":"chat.completion","created":1,"model":"gpt-4o","choices":[{"index":0,"message":{"role":"assistant","content":"hi there","refusal":null,"annotations":[]},"logprobs":null,"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10},"service_tier":"default","system_fingerprint":"fp_plain"}"#;
const PLAIN_SSE: &str = concat!(
    "data: {\"id\":\"chatcmpl-plain\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"service_tier\":\"default\",\"system_fingerprint\":\"fp_plain\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi there\",\"refusal\":null},\"logprobs\":null,\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-plain\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"service_tier\":\"default\",\"system_fingerprint\":\"fp_plain\",\"choices\":[{\"index\":0,\"delta\":{},\"logprobs\":null,\"finish_reason\":\"stop\"}]}\n\n",
    "data: {\"id\":\"chatcmpl-plain\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"service_tier\":\"default\",\"system_fingerprint\":\"fp_plain\",\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"total_tokens\":10}}\n\n",
    "data: [DONE]\n\n"
);

#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<(String, HeaderMap, Bytes)>>,
    rejection: Mutex<Option<(u16, Value)>>,
}

fn reply(
    status: u16,
    mime: &str,
    body: impl Into<Body>,
    request_id: &str,
    retry: &str,
) -> Response {
    let mut response = Response::builder()
        .status(status)
        .header("content-type", mime)
        .body(body.into())
        .unwrap();
    for (name, value) in [
        ("x-request-id", request_id),
        ("openai-processing-ms", "13.25"),
        ("x-ratelimit-remaining-tokens", "71"),
        ("retry-after", retry),
        ("set-cookie", "secret=hidden"),
        ("authorization", "Bearer hidden"),
        ("x-provider-secret", "hidden"),
        ("connection", "close"),
    ] {
        response.headers_mut().insert(name, value.parse().unwrap());
    }
    response
}
fn error_body(status: u16) -> String {
    format!(
        r#"{{ "error":{{"message":"native validation {status}","type":"invalid_request_error","param":"deliberately_invalid","code":"provider_code","future":null}}, "request_id":"error-{status}" }}"#
    )
}
async fn upstream(State(recorder): State<Arc<Recorder>>, request: Request<Body>) -> Response {
    let path = request.uri().path().to_owned();
    let method = request.method().clone();
    let wants_stream = request
        .headers()
        .get("accept")
        .is_some_and(|value| value == "text/event-stream");
    let wire_path = request.uri().path_and_query().unwrap().to_string();
    let headers = request.headers().clone();
    if path.ends_with("/messages") {
        assert_eq!(headers["x-api-key"], "provider-native-key");
    } else {
        assert_eq!(headers["authorization"], "Bearer provider-native-key");
    }
    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    recorder
        .calls
        .lock()
        .await
        .push((wire_path, headers, bytes));
    let model = value["model"].as_str().unwrap_or("gpt-4o");
    if model.contains("reject-fixture") {
        if let Some((status, body)) = recorder.rejection.lock().await.as_ref() {
            return reply(
                *status,
                "application/json",
                body.to_string(),
                "fixture-validation",
                "3",
            );
        }
    }
    if path.ends_with("/messages") {
        // Record validation failures before returning them: a panic/network
        // error must never masquerade as an eligible fallback.
        let reasoning_error = if value["thinking"]["type"] == "adaptive"
            && model.contains("reject-adaptive")
        {
            Some("adaptive thinking is not supported on this model")
        } else if value["thinking"]["type"] == "disabled" && model.contains("reject-disabled") {
            Some(
                r#""thinking.type.disabled" is not supported for this model. Use "thinking.type.adaptive" and "output_config.effort" to control thinking behavior."#,
            )
        } else if model.contains("reject-level") {
            // Reported wire diagnostic: https://github.com/diegosouzapw/OmniRoute/issues/7044
            Some("This model does not support effort level 'xhigh'. Supported levels: high, low, max, medium.")
        } else if model.contains("reject-effort") {
            // Captured provider diagnostic: https://github.com/can1357/oh-my-pi/issues/3497
            Some("This model does not support the effort parameter.")
        } else if value["output_config"]["effort"] == "bogus" {
            Some("invalid output_config.effort")
        } else {
            None
        };
        if let Some(message) = reasoning_error {
            return reply(
                400,
                "application/json",
                json!({"type":"error","error":{"type":"invalid_request_error","message":message}})
                    .to_string(),
                "anthropic-validation",
                "3",
            );
        }
        if value["stream"] == true {
            if model.contains("late-error") {
                return reply(200, "text/event-stream", concat!(
                    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_mock\",\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
                    "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"adaptive thinking is not supported on this model\"}}\n\n"
                ), "anthropic-stream", "3");
            }
            return reply(
                200,
                "text/event-stream",
                concat!(
                    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_mock\",\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
                    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
                    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
                    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
                ),
                "anthropic-stream",
                "3",
            );
        }
        return reply(
            200,
            "application/json",
            json!({"id":"msg_mock","type":"message","role":"assistant","model":model,
                "content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn",
                "usage":{"input_tokens":3,"output_tokens":2}})
            .to_string(),
            "anthropic-json",
            "3",
        );
    }
    let status = model
        .strip_prefix("reject")
        .and_then(|value| value.parse::<u16>().ok())
        .or_else(|| {
            (value.get("deliberately_invalid").is_some()
                || value.get("temperature").is_some_and(Value::is_string))
            .then_some(400)
        });
    if let Some(status) = status {
        return reply(
            status,
            "application/json; charset=utf-8",
            error_body(status),
            &format!("error-{status}"),
            if status == 503 {
                "Wed, 21 Oct 2037 07:28:00 GMT"
            } else {
                "3"
            },
        );
    }
    if method == "DELETE" && path == "/responses/resp-native" {
        return reply(
            202,
            "application/json; charset=utf-8",
            DELETED_REPLY,
            "responses-delete",
            "3",
        );
    }
    match path.as_str() {
        "/responses" if model == "malformed" => {
            reply(200, "application/json", "{broken", "malformed", "3")
        }
        "/responses" if model == "bad-mime" => {
            reply(200, "application/json", RESPONSES_REPLY, "bad-mime", "3")
        }
        "/responses" | "/responses/resp-native" if value["stream"] == true || wants_stream => {
            let body = responses_sse();
            let chunks: Vec<_> = body
                .as_bytes()
                .chunks(7)
                .map(|chunk| Ok::<_, std::convert::Infallible>(Bytes::copy_from_slice(chunk)))
                .collect();
            reply(
                201,
                "text/event-stream; charset=utf-8",
                Body::from_stream(futures::stream::iter(chunks)),
                "responses-stream",
                "3",
            )
        }
        "/responses" | "/responses/resp-native" | "/responses/compact" => reply(
            201,
            "application/json; charset=utf-8",
            RESPONSES_REPLY,
            "responses-json",
            "3",
        ),
        "/responses/resp-native/cancel" => reply(
            202,
            "application/json; charset=utf-8",
            RESPONSES_REPLY.replace("completed", "cancelled"),
            "responses-cancel",
            "3",
        ),
        "/responses/resp-empty" => reply(204, "application/json", "", "responses-empty", "3"),
        "/chat/completions" if model == "plain" => {
            if value["stream"] == true {
                reply(200, "text/event-stream", PLAIN_SSE, "plain-stream", "3")
            } else {
                reply(200, "application/json", PLAIN_REPLY, "plain-json", "3")
            }
        }
        "/chat/completions" if value["stream"] == true && model == "created" => reply(
            201,
            "text/event-stream; charset=utf-8",
            PLAIN_SSE,
            "created-stream",
            "3",
        ),
        "/chat/completions" if value["stream"] == true => {
            // Split in the middle of lines and JSON, exercising incremental
            // usage filtering without introducing timing dependencies.
            let chunks: Vec<_> = SSE_REPLY
                .as_bytes()
                .chunks(7)
                .map(|chunk| Ok::<_, std::convert::Infallible>(Bytes::copy_from_slice(chunk)))
                .collect();
            reply(
                200,
                "text/event-stream",
                Body::from_stream(futures::stream::iter(chunks)),
                "stream-native",
                "3",
            )
        }
        "/chat/completions" if model == "created" => reply(
            201,
            "application/json; charset=utf-8",
            RICH_REPLY,
            "created-native",
            "3",
        ),
        "/chat/completions" => reply(200, "application/json", RICH_REPLY, "chat-native", "3"),
        "/embeddings" => reply(
            200,
            "application/json",
            EMBEDDING_REPLY,
            "embedding-native",
            "3",
        ),
        "/images/generations" => reply(200, "application/json", IMAGE_REPLY, "image-native", "3"),
        other => panic!("unexpected native upstream path: {other}"),
    }
}

struct RecordingKeys {
    inner: Arc<dyn ApiKeyStore>,
    usage: Mutex<Vec<(u64, u64, u64)>>,
}
#[async_trait]
impl ApiKeyStore for RecordingKeys {
    async fn insert_raw(&self, key: ApiKey) -> Result<(), ApiKeyStoreError> {
        self.inner.insert_raw(key).await
    }
    async fn create(
        &self,
        description: String,
        permission: ApiKeyPermission,
        labels: HashMap<String, String>,
        pool: Option<String>,
    ) -> Result<CreatedApiKey, ApiKeyStoreError> {
        self.inner
            .create(description, permission, labels, pool)
            .await
    }
    async fn update(
        &self,
        hash: &str,
        description: Option<String>,
        pool: Option<Option<String>>,
        permission: Option<ApiKeyPermission>,
    ) -> Result<Option<Arc<ApiKey>>, ApiKeyStoreError> {
        self.inner.update(hash, description, pool, permission).await
    }
    async fn authenticate(&self, token: &str) -> Option<Arc<ApiKey>> {
        self.inner.authenticate(token).await
    }
    async fn get(&self, hash: &str) -> Option<Arc<ApiKey>> {
        self.inner.get(hash).await
    }
    async fn list(&self) -> Vec<Arc<ApiKey>> {
        self.inner.list().await
    }
    async fn list_page(
        &self,
        cursor: Option<&StablePageCursor>,
        fetch: i64,
    ) -> Result<Vec<Arc<ApiKey>>, ApiKeyStoreError> {
        self.inner.list_page(cursor, fetch).await
    }
    async fn select_by_labels(
        &self,
        labels: &HashMap<String, String>,
        required: Option<&str>,
        hashes: &[String],
        fetch: i64,
    ) -> Result<Vec<Arc<ApiKey>>, ApiKeyStoreError> {
        self.inner
            .select_by_labels(labels, required, hashes, fetch)
            .await
    }
    async fn revoke(&self, hash: &str) -> Result<bool, ApiKeyStoreError> {
        self.inner.revoke(hash).await
    }
    async fn record_usage(&self, _: &str, input: u64, output: u64, total: u64) {
        self.usage.lock().await.push((input, output, total));
    }
}

struct Setup {
    app: Router,
    state: AppState,
    recorder: Arc<Recorder>,
    cache: Arc<MemoryPromptCache>,
    keys: Arc<RecordingKeys>,
    key: String,
    /// Caller-supplied `fallbacks` override per concrete primary target
    /// (`connection_id/model`); empty when the test names no alternatives.
    fallbacks: Vec<(String, Vec<String>)>,
    native_id: String,
    translated_id: String,
    routes: Arc<InMemoryModelRouteStore>,
}
async fn boot(
    cache_enabled: bool,
    fallbacks: &[&str],
    permission: Option<ApiKeyPermission>,
) -> Setup {
    boot_with_translated_provider(cache_enabled, fallbacks, permission, "anthropic").await
}

async fn boot_with_translated_provider(
    cache_enabled: bool,
    fallbacks: &[&str],
    permission: Option<ApiKeyPermission>,
    provider_kind: &str,
) -> Setup {
    let recorder = Arc::new(Recorder::default());
    let base_url = serve(
        Router::new()
            .fallback(upstream)
            .with_state(recorder.clone()),
    )
    .await;
    let connections = Arc::new(InMemoryProviderConnectionStore::new());
    let native = connections
        .create(ConnectionInput {
            name: "native-wire".into(),
            provider_kind: "openai".into(),
            base_url: Some(base_url.clone()),
            prefix: Some("native/".into()),
            exposure: ExposurePolicy::Dynamic,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: "provider-native-key".into(),
        })
        .await
        .unwrap();
    let translated = connections
        .create(ConnectionInput {
            name: "translated-wire".into(),
            provider_kind: provider_kind.into(),
            base_url: Some(base_url.clone()),
            prefix: Some("translated/".into()),
            exposure: ExposurePolicy::Dynamic,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: "provider-native-key".into(),
        })
        .await
        .unwrap();
    let routes = Arc::new(InMemoryModelRouteStore::new());
    let backend: Arc<dyn BackendProvider> = Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        connections.clone(),
        routes.clone(),
    ));
    let store = Arc::new(RecordingKeys {
        inner: Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
            b"native-wire-pepper".to_vec(),
        ))),
        usage: Mutex::new(Vec::new()),
    });
    let mut permission = permission.unwrap_or_default();
    if cache_enabled {
        permission.cache = Some(CachePolicy::new(CacheScope::Token));
    }
    let key = store
        .create("native wire".into(), permission, HashMap::new(), None)
        .await
        .unwrap()
        .raw_key;
    let cache = Arc::new(MemoryPromptCache::new(64, 8 << 20));
    // Fallbacks travel as a per-request override keyed by the primary's
    // concrete `connection_id/model` target.
    let concrete = |value: &str| match value.split_once('/') {
        Some(("native", model)) => format!("{}/{model}", native.id),
        Some(("translated", model)) => format!("{}/{model}", translated.id),
        _ => value.to_string(),
    };
    let alternatives: Vec<String> = fallbacks.iter().map(|value| concrete(value)).collect();
    let fallbacks = if alternatives.is_empty() {
        Vec::new()
    } else {
        ["native/reject500", "translated/claude-3-haiku-20240307"]
            .iter()
            .map(|primary| (concrete(primary), alternatives.clone()))
            .collect()
    };
    let state = AppStateBuilder::new(backend)
        .provider_connection_store(connections)
        .model_route_store(routes.clone())
        .fallback_policy(if fallbacks.is_empty() {
            FallbackPolicy::empty()
        } else {
            FallbackPolicy::from_map(
                [
                    "native/reject400",
                    "native/reject500",
                    "native/reject503",
                    "native/malformed",
                    "native/bad-mime",
                ]
                .into_iter()
                .map(|primary| (primary.to_string(), alternatives.clone()))
                .collect(),
            )
        })
        .api_key_store(store.clone())
        .prompt_cache(cache.clone())
        .build()
        .await;
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(handlers::chat::create_chat_completion),
        )
        .route("/v1/responses", post(handlers::responses::create_response))
        .route("/v1/messages", post(handlers::anthropic::proxy_messages))
        .route(
            "/v1/responses/compact",
            post(handlers::responses::compact_response),
        )
        .route(
            "/v1/responses/:response_id",
            get(handlers::responses::retrieve_response)
                .delete(handlers::responses::delete_response),
        )
        .route(
            "/v1/responses/:response_id/cancel",
            post(handlers::responses::cancel_response),
        )
        .route(
            "/v1/embeddings",
            post(handlers::embeddings::create_embedding),
        )
        .route(
            "/v1/images/generations",
            post(handlers::images::create_image),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state.clone());
    Setup {
        app,
        state,
        recorder,
        cache,
        keys: store,
        key,
        fallbacks,
        native_id: native.id.clone(),
        translated_id: translated.id.clone(),
        routes,
    }
}
async fn send(
    setup: &Setup,
    path: &str,
    body: impl Into<Body>,
    beta: Option<&str>,
) -> (StatusCode, HeaderMap, Bytes) {
    let mut body = body.into();
    if path == "/v1/chat/completions" && !setup.fallbacks.is_empty() {
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let mut value: Value = serde_json::from_slice(&bytes).unwrap();
        let model = value["model"].as_str().unwrap_or_default().to_owned();
        let primary = match model.split_once('/') {
            Some((prefix, upstream)) => setup
                .fallbacks
                .iter()
                .find(|(primary, _)| primary.ends_with(&format!("/{upstream}")))
                .filter(|_| matches!(prefix, "native" | "translated")),
            None => None,
        };
        if let Some((primary, alternatives)) = primary {
            value["fallbacks"] = json!({ primary: alternatives });
        }
        body = Body::from(value.to_string());
    }
    let mut request = Request::post(path)
        .header("authorization", format!("Bearer {}", setup.key))
        .header("content-type", "application/json");
    if let Some(beta) = beta {
        request = request.header("openai-beta", beta);
    }
    let response = setup
        .app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, bytes)
}
fn chat(model: &str, stream: bool) -> String {
    json!({"model":model,"messages":[{"role":"user","content":"hello"}],"stream":stream})
        .to_string()
}
fn assert_headers(headers: &HeaderMap, id: &str, retry: &str) {
    assert_eq!(headers["x-request-id"], id);
    assert_eq!(headers["openai-processing-ms"], "13.25");
    assert_eq!(headers["x-ratelimit-remaining-tokens"], "71");
    assert_eq!(headers["retry-after"], retry);
    for forbidden in [
        "authorization",
        "set-cookie",
        "x-provider-secret",
        "content-encoding",
    ] {
        assert!(!headers.contains_key(forbidden), "leaked {forbidden}");
    }
    assert_ne!(
        headers
            .get("connection")
            .and_then(|value| value.to_str().ok()),
        Some("close"),
        "upstream hop-by-hop header leaked"
    );
}
async fn cache_ready(setup: &Setup) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while setup.cache.len() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cache write completes");
}

#[tokio::test]
async fn native_chat_forwards_new_unknown_and_malformed_controls_in_both_modes() {
    let setup = boot(false, &[], None).await;
    for stream in [false, true] {
        let body = format!(
            r#"{{"model":"native/gpt-4o","messages":[{{"role":"user","content":"hello","future_message":null}}],"stream":{stream},"reasoning_effort":"high","verbosity":"low","service_tier":"priority","logprobs":true,"top_logprobs":2,"future_option":{{"decimal":1.0000000000000001,"null":null}},"max_tokens":11}}"#
        );
        let (status, _, _) =
            send(&setup, "/v1/chat/completions", body, Some("future-mode=v1")).await;
        assert_eq!(status, 200);
        let calls = setup.recorder.calls.lock().await;
        let (_, headers, bytes) = calls.last().unwrap();
        let value: Value = serde_json::from_slice(bytes).unwrap();
        for (name, expected) in [
            ("reasoning_effort", json!("high")),
            ("verbosity", json!("low")),
            ("service_tier", json!("priority")),
            ("logprobs", json!(true)),
            ("top_logprobs", json!(2)),
        ] {
            assert_eq!(value[name], expected);
        }
        assert_eq!(value["messages"][0]["future_message"], Value::Null);
        assert_eq!(value["max_completion_tokens"], 11);
        assert!(value.get("max_tokens").is_none());
        assert_eq!(headers["openai-beta"], "future-mode=v1");
        assert!(std::str::from_utf8(bytes)
            .unwrap()
            .contains("1.0000000000000001"));
    }
    for stream in [false, true] {
        for field in [r#""deliberately_invalid":true"#, r#""temperature":"bad""#] {
            let body = format!(
                r#"{{"model":"native/gpt-4o","messages":[{{"role":"user","content":"hello"}}],"stream":{stream},{field}}}"#
            );
            let (status, _, bytes) = send(&setup, "/v1/chat/completions", body, None).await;
            assert_eq!(status, 400);
            assert_eq!(bytes, error_body(400));
        }
    }
    assert_eq!(
        setup.recorder.calls.lock().await.len(),
        6,
        "invalid controls reached provider validation"
    );
}

#[tokio::test]
async fn native_chat_preserves_reply_bytes_nulls_numbers_refusal_usage_and_headers() {
    let setup = boot(false, &[], None).await;
    let (status, headers, bytes) = send(
        &setup,
        "/v1/chat/completions",
        chat("native/gpt-4o", false),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(bytes, RICH_REPLY);
    assert_headers(&headers, "chat-native", "3");
}

#[tokio::test]
async fn native_chat_and_responses_preserve_error_bodies_status_and_retry_headers() {
    let setup = boot(false, &[], None).await;
    for status in [400, 429, 503] {
        for path in ["/v1/chat/completions", "/v1/responses"] {
            let body = if path.ends_with("responses") {
                json!({"model":format!("native/reject{status}"),"input":"hello"}).to_string()
            } else {
                chat(&format!("native/reject{status}"), false)
            };
            let (actual, headers, bytes) = send(&setup, path, body, None).await;
            assert_eq!(
                actual.as_u16(),
                status,
                "{path}: {}",
                String::from_utf8_lossy(&bytes)
            );
            assert_eq!(bytes, error_body(status));
            assert_eq!(headers["content-type"], "application/json; charset=utf-8");
            assert_headers(
                &headers,
                &format!("error-{status}"),
                if status == 503 {
                    "Wed, 21 Oct 2037 07:28:00 GMT"
                } else {
                    "3"
                },
            );
        }
    }
}

#[tokio::test]
async fn native_fallback_returns_only_the_winner_and_last_dispatched_error() {
    let setup = boot(false, &["native/gpt-4o"], None).await;
    for stream in [false, true] {
        let (status, headers, bytes) = send(
            &setup,
            "/v1/chat/completions",
            chat("native/reject500", stream),
            None,
        )
        .await;
        assert_eq!(status, 200);
        if stream {
            assert!(std::str::from_utf8(&bytes).unwrap().contains("\"refusal\""));
        } else {
            assert_eq!(bytes, RICH_REPLY);
        }
        assert_headers(
            &headers,
            if stream {
                "stream-native"
            } else {
                "chat-native"
            },
            "3",
        );
        // Fallback destinations are reported by concrete connection target.
        assert_eq!(
            headers["x-chalk-routed-model"],
            format!("{}/gpt-4o", setup.native_id).as_str()
        );
    }
    assert_eq!(setup.recorder.calls.lock().await.len(), 4);
    let setup = boot(false, &["native/reject429", "native/reject503"], None).await;
    for stream in [false, true] {
        let (status, headers, bytes) = send(
            &setup,
            "/v1/chat/completions",
            chat("native/reject500", stream),
            None,
        )
        .await;
        assert_eq!(status, 503);
        assert_eq!(bytes, error_body(503));
        assert_headers(&headers, "error-503", "Wed, 21 Oct 2037 07:28:00 GMT");
    }
    assert_eq!(setup.recorder.calls.lock().await.len(), 6);
}

#[tokio::test]
async fn forbidden_fallback_does_not_replace_the_dispatched_provider_error() {
    let setup = boot(
        false,
        &["native/gpt-4o"],
        Some(ApiKeyPermission {
            models: Some(vec!["native/reject500".into()]),
            ..Default::default()
        }),
    )
    .await;
    let (status, headers, bytes) = send(
        &setup,
        "/v1/chat/completions",
        chat("native/reject500", false),
        None,
    )
    .await;
    assert_eq!(status, 500);
    assert_eq!(bytes, error_body(500));
    assert_headers(&headers, "error-500", "3");
    assert_eq!(setup.recorder.calls.lock().await.len(), 1);
}

#[tokio::test]
async fn streamed_usage_visibility_respects_client_choice_and_accounting_observes_usage() {
    for requested in [None, Some(false), Some(true)] {
        let setup = boot(false, &[], None).await;
        let mut body: Value = serde_json::from_str(&chat("native/gpt-4o", true)).unwrap();
        if let Some(include_usage) = requested {
            body["stream_options"] = json!({"include_usage":include_usage,"future_option":"kept"});
        }
        let (status, headers, bytes) =
            send(&setup, "/v1/chat/completions", body.to_string(), None).await;
        assert_eq!(status, 200);
        assert_headers(&headers, "stream-native", "3");
        let wire = std::str::from_utf8(&bytes).unwrap();
        assert!(wire.contains("provider-comment"));
        assert!(wire.contains("before-usage"));
        assert!(wire.contains("id: native-event"));
        assert!(wire.contains("\"refusal\":\"Cannot help\""));
        assert!(wire.contains("[DONE]"));
        assert_eq!(wire.contains("\"usage\""), requested == Some(true));
        if requested == Some(true) {
            assert_eq!(wire, SSE_REPLY);
        }
        let calls = setup.recorder.calls.lock().await;
        let value: Value = serde_json::from_slice(&calls[0].2).unwrap();
        assert_eq!(value["stream_options"]["include_usage"], true);
        if requested.is_some() {
            assert_eq!(value["stream_options"]["future_option"], "kept");
        }
        drop(calls);
        assert_eq!(
            setup.keys.usage.lock().await.as_slice(),
            &[(7, 3, 10)],
            "internal terminal usage must settle once regardless of client visibility"
        );
    }
}

#[tokio::test]
async fn rich_nonstream_cache_is_exact_and_streaming_conversion_bypasses_it() {
    let setup = boot(true, &[], None).await;
    let body = chat("native/gpt-4o", false);
    let (_, _, first) = send(&setup, "/v1/chat/completions", body.clone(), None).await;
    assert_eq!(first, RICH_REPLY);
    cache_ready(&setup).await;
    let (_, headers, second) = send(&setup, "/v1/chat/completions", body, None).await;
    assert_eq!(second, RICH_REPLY);
    assert_eq!(headers["x-chalk-cache"], "hit");
    assert_eq!(
        setup.keys.usage.lock().await.as_slice(),
        &[(7, 3, 10)],
        "cache hit must not write upstream usage again"
    );
    for stale in [
        "x-request-id",
        "retry-after",
        "openai-processing-ms",
        "x-ratelimit-remaining-tokens",
    ] {
        assert!(!headers.contains_key(stale));
    }
    let (_, _, third) = send(
        &setup,
        "/v1/chat/completions",
        chat("native/gpt-4o", true),
        None,
    )
    .await;
    assert!(std::str::from_utf8(&third).unwrap().contains("\"refusal\""));
    assert_eq!(setup.recorder.calls.lock().await.len(), 2);
}

#[tokio::test]
async fn openai_shaped_streams_with_metadata_and_null_fields_are_cached_and_replayed() {
    let setup = boot(true, &[], None).await;
    let (_, headers, first) = send(
        &setup,
        "/v1/chat/completions",
        chat("native/plain", true),
        None,
    )
    .await;
    assert_eq!(headers["x-chalk-cache"], "miss");
    assert!(std::str::from_utf8(&first).unwrap().contains("hi there"));
    cache_ready(&setup).await;
    for stream in [true, false] {
        let (status, headers, bytes) = send(
            &setup,
            "/v1/chat/completions",
            chat("native/plain", stream),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["x-chalk-cache"], "hit", "stream={stream}");
        assert!(std::str::from_utf8(&bytes).unwrap().contains("hi there"));
    }
    assert_eq!(
        setup.recorder.calls.lock().await.len(),
        1,
        "replays must not reach the provider"
    );
}

#[tokio::test]
async fn rich_streams_unknown_or_side_effecting_controls_never_produce_weaker_cache_replays() {
    let setup = boot(true, &[], None).await;
    for _ in 0..2 {
        let (_, _, bytes) = send(
            &setup,
            "/v1/chat/completions",
            chat("native/gpt-4o", true),
            None,
        )
        .await;
        assert!(std::str::from_utf8(&bytes).unwrap().contains("\"refusal\""));
    }
    assert_eq!(setup.cache.len(), 0);
    for extra in [
        json!({"store":true}),
        json!({"metadata":{"tag":"side-effect"}}),
        json!({"future_option":true}),
        json!({"stream_options":{"include_usage":false,"future_option":true}}),
        json!({"messages":[{"role":"user","content":"hello","future_message":null}]}),
        json!({"tools":[{"type":"function","function":{"name":"x","parameters":{},"future_tool":true}}]}),
    ] {
        let mut body: Value = serde_json::from_str(&chat("native/gpt-4o", false)).unwrap();
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        for _ in 0..2 {
            let (_, headers, bytes) =
                send(&setup, "/v1/chat/completions", body.to_string(), None).await;
            assert_eq!(bytes, RICH_REPLY);
            assert!(!headers.contains_key("x-chalk-cache"));
        }
    }
    assert_eq!(setup.recorder.calls.lock().await.len(), 14);
    assert_eq!(setup.cache.len(), 0);
}

#[tokio::test]
async fn forwarded_beta_headers_partition_cache_identity() {
    let setup = boot(true, &[], None).await;
    let body = chat("native/gpt-4o", false);
    send(
        &setup,
        "/v1/chat/completions",
        body.clone(),
        Some("future=v1"),
    )
    .await;
    cache_ready(&setup).await;
    let (_, headers, _) = send(
        &setup,
        "/v1/chat/completions",
        body.clone(),
        Some("future=v1"),
    )
    .await;
    assert_eq!(headers["x-chalk-cache"], "hit");
    send(&setup, "/v1/chat/completions", body, Some("future=v2")).await;
    assert_eq!(setup.recorder.calls.lock().await.len(), 2);
}

#[tokio::test]
async fn native_embeddings_and_images_preserve_requests_exact_numbers_reply_metadata_and_headers() {
    let setup = boot(false, &[], None).await;
    for (path, body, expected, id) in [
        (
            "/v1/embeddings",
            r#"{"model":"native/text-embedding-3-small","input":[[1,2],[3]],"future_option":{"decimal":1.0000000000000001}}"#,
            EMBEDDING_REPLY,
            "embedding-native",
        ),
        (
            "/v1/images/generations",
            r#"{"model":"native/gpt-image-1","prompt":"hello","moderation":"low","future_option":{"decimal":1.0000000000000001}}"#,
            IMAGE_REPLY,
            "image-native",
        ),
    ] {
        let (status, headers, bytes) = send(&setup, path, body, None).await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
        assert_eq!(bytes, expected);
        assert_headers(&headers, id, "3");
        let calls = setup.recorder.calls.lock().await;
        assert!(std::str::from_utf8(&calls.last().unwrap().2)
            .unwrap()
            .contains("1.0000000000000001"));
    }
    let request: CreateEmbeddingRequest =
        serde_json::from_str(r#"{"model":"m","input":[[1,2],[3]]}"#).unwrap();
    assert!(matches!(request.input, EmbeddingInput::TokenBatch(_)));
    assert_eq!(request.input.token_count(), Some(3));
    let (status, _, _) = send(
        &setup,
        "/v1/images/generations",
        r#"{"model":"native/gpt-image-1","prompt":"hello","stream":true}"#,
        None,
    )
    .await;
    assert_eq!(status, 501);
    assert_eq!(
        setup.recorder.calls.lock().await.len(),
        2,
        "unsupported streaming images fail before dispatch"
    );
}

#[tokio::test]
async fn ambiguous_routing_fields_are_rejected_but_unfamiliar_raw_subtrees_survive() {
    let setup = boot(false, &[], None).await;
    for body in [
        r#"{"model":"native/gpt-4o","model":"native/other","messages":[]}"#,
        r#"{"model":"native/gpt-4o","messages":[],"stream":"bad"}"#,
        r#"{"model":"native/gpt-4o","messages":[{"role":"user","content":"hello","content":"other"}]}"#,
        r#"{"model":"native/gpt-4o","messages":[],"stream_options":{"include_usage":true,"include_usage":false}}"#,
        r#"{"model":"native/gpt-4o","messages":[],"tools":[{"type":"function","function":{"name":"x","parameters":{"type":"object","properties":{"x":{},"x":{}}}}}]}"#,
    ] {
        let (status, _, _) = send(&setup, "/v1/chat/completions", body, None).await;
        assert_eq!(status, 422);
    }
    let (status, _, _) = send(
        &setup,
        "/v1/embeddings",
        r#"{"model":"native/text-embedding-3-small","model":"other","input":"hello"}"#,
        None,
    )
    .await;
    assert_eq!(status, 422);
    assert_eq!(setup.recorder.calls.lock().await.len(), 0);
    let body = r#"{"model":"native/gpt-4o","messages":[],"future_option":{"x":1,"x":2}}"#;
    let native: NativeChatRequest = serde_json::from_str(body).unwrap();
    assert!(serde_json::to_string(&native)
        .unwrap()
        .contains(r#""future_option":{"x":1,"x":2}"#));
    assert!(native.cache_fields().is_none());
}

#[tokio::test]
async fn incompatible_translated_primary_is_skipped_for_native_fallback_without_dropping_controls()
{
    let setup = boot(false, &["native/gpt-4o"], None).await;
    for (field, value) in [("n", json!(2)), ("presence_penalty", json!(0.5))] {
        for stream in [false, true] {
            let body = json!({"model":"translated/claude-3-haiku-20240307","messages":[{"role":"user","content":"hello"}],"stream":stream,field:value});
            let (status, headers, bytes) =
                send(&setup, "/v1/chat/completions", body.to_string(), None).await;
            assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
            // Fallback destinations are reported by concrete connection target.
            assert_eq!(
                headers["x-chalk-routed-model"],
                format!("{}/gpt-4o", setup.native_id).as_str()
            );
            let calls = setup.recorder.calls.lock().await;
            assert_eq!(calls.last().unwrap().0, "/chat/completions");
            let sent: Value = serde_json::from_slice(&calls.last().unwrap().2).unwrap();
            assert_eq!(sent[field], value);
        }
    }
    assert_eq!(
        setup.recorder.calls.lock().await.len(),
        4,
        "incompatible primary never dispatches"
    );
}

async fn reasoning_route(setup: &Setup, primary: &str, connection: &str, fallback: &str) -> String {
    reasoning_route_from(setup, &setup.translated_id, primary, connection, fallback).await
}

async fn reasoning_route_from(
    setup: &Setup,
    primary_connection: &str,
    primary: &str,
    connection: &str,
    fallback: &str,
) -> String {
    let alias = format!("chalk/reasoning-{}", uuid::Uuid::new_v4());
    setup
        .routes
        .create(RouteInput {
            public_model_id: alias.clone(),
            connection_id: primary_connection.to_string(),
            upstream_model_id: primary.to_string(),
            published: true,
            routing: Some(openai_server::route_grid::RouteGrid {
                default_target: openai_server::route_grid::RouteTarget {
                    connection_id: primary_connection.to_string(),
                    upstream_model_id: primary.to_string(),
                },
                judge: None,
                fallbacks: std::collections::BTreeMap::from([(
                    format!("{primary_connection}/{primary}"),
                    vec![format!("{connection}/{fallback}")],
                )]),
            }),
        })
        .await
        .unwrap();
    setup.state.backend.invalidate_routing_cache().await;
    alias
}

fn reasoning_body(path: &str, model: &str, effort: Option<&str>, stream: bool) -> Value {
    let mut body = if path == "/v1/responses" {
        json!({"model": model, "input": "hello", "stream": stream})
    } else {
        json!({"model": model, "messages": [{"role": "user", "content": "hello"}], "stream": stream})
    };
    if let Some(effort) = effort {
        if path == "/v1/responses" {
            body["reasoning"] = json!({"effort": effort});
        } else {
            body["reasoning_effort"] = json!(effort);
        }
    }
    body
}

#[tokio::test]
async fn future_reasoning_models_and_efforts_reach_upstream_without_declarations() {
    for provider in ["anthropic", "bedrock"] {
        let setup = boot_with_translated_provider(false, &[], None, provider).await;
        let upstream = if provider == "bedrock" {
            "us.anthropic.claude-future-6-v1:0"
        } else {
            "claude-future-6"
        };
        let alias = reasoning_route(&setup, upstream, &setup.native_id, "gpt-4o").await;
        for effort in [
            None,
            Some("none"),
            Some("minimal"),
            Some("high"),
            Some("xhigh"),
            Some("max"),
            Some("future-effort"),
        ] {
            for path in ["/v1/chat/completions", "/v1/responses"] {
                for stream in [false, true] {
                    setup.recorder.calls.lock().await.clear();
                    let body = reasoning_body(path, &alias, effort, stream);
                    let (status, _, bytes) = send(&setup, path, body.to_string(), None).await;
                    assert_eq!(
                        status,
                        200,
                        "{provider} {path} {effort:?} {stream}: {}",
                        String::from_utf8_lossy(&bytes)
                    );
                    assert!(String::from_utf8_lossy(&bytes).contains("hello"));
                    let calls = setup.recorder.calls.lock().await;
                    assert_eq!(calls.len(), 1, "primary must serve without fallback");
                    assert!(calls[0].0.ends_with("/messages"));
                    let sent: Value = serde_json::from_slice(&calls[0].2).unwrap();
                    assert_eq!(sent["model"], upstream);
                    match effort {
                        None => {
                            assert!(sent.get("thinking").is_none());
                            assert!(sent.get("output_config").is_none());
                        }
                        Some("none") => {
                            assert_eq!(sent["thinking"], json!({"type":"disabled"}));
                            assert!(sent.get("output_config").is_none());
                        }
                        Some(effort) => {
                            assert_eq!(sent["thinking"], json!({"type":"adaptive"}));
                            assert_eq!(
                                sent["output_config"]["effort"],
                                if effort == "minimal" { "low" } else { effort }
                            );
                        }
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn translated_reasoning_invalid_effort_is_terminal() {
    for provider in ["anthropic", "bedrock"] {
        let setup = boot_with_translated_provider(false, &[], None, provider).await;
        let model = "claude-future-6";
        let effort = "bogus";
        let message = "invalid output_config.effort";
        let upstream = if provider == "bedrock" {
            format!("us.anthropic.{model}-v1:0")
        } else {
            model.to_string()
        };
        let alias = reasoning_route(&setup, &upstream, &setup.native_id, "gpt-4o").await;
        for public_model in [alias, format!("translated/{upstream}")] {
            for path in ["/v1/chat/completions", "/v1/responses"] {
                for stream in [false, true] {
                    setup.recorder.calls.lock().await.clear();
                    let body = reasoning_body(path, &public_model, Some(effort), stream);
                    let (status, headers, bytes) = send(&setup, path, body.to_string(), None).await;
                    assert_eq!(
                        status,
                        400,
                        "{provider} {model} {effort} {path} {stream}: {}",
                        String::from_utf8_lossy(&bytes)
                    );
                    assert_eq!(headers["content-type"], "application/json");
                    let error: Value = serde_json::from_slice(&bytes).unwrap();
                    assert!(
                        error["error"]["message"]
                            .as_str()
                            .unwrap()
                            .contains(message),
                        "{error}"
                    );
                    let calls = setup.recorder.calls.lock().await;
                    assert_eq!(
                        calls.len(),
                        1,
                        "provider 400 must not retry or invoke fallback"
                    );
                    assert!(calls[0].0.ends_with("/messages"));
                    let sent: Value = serde_json::from_slice(&calls[0].2).unwrap();
                    assert_eq!(sent["model"], upstream);
                    assert_eq!(sent["thinking"], json!({"type":"adaptive"}));
                    assert_eq!(sent["output_config"]["effort"], effort);
                }
            }
        }
    }
}

// Thinking diagnostics are documented at https://platform.claude.com/docs/en/api/errors.
// Effort diagnostics below are reported wire errors, not a model-support catalog.
#[tokio::test]
async fn translated_reasoning_support_rejections_fall_back_without_weakening_controls() {
    for provider in ["anthropic", "bedrock"] {
        let setup = boot_with_translated_provider(false, &[], None, provider).await;
        for (model, effort, message) in [
            (
                "claude-reject-adaptive",
                "minimal",
                "adaptive thinking is not supported on this model",
            ),
            (
                "claude-reject-disabled",
                "none",
                r#""thinking.type.disabled" is not supported for this model."#,
            ),
            (
                "claude-reject-effort",
                "future-depth",
                "This model does not support the effort parameter.",
            ),
            (
                "claude-reject-level",
                "xhigh",
                "This model does not support effort level 'xhigh'. Supported levels: high, low, max, medium.",
            ),
        ] {
            let upstream = if provider == "bedrock" {
                format!("us.anthropic.{model}-v1:0")
            } else {
                model.to_string()
            };
            // Exercise native and translated destinations; both retain the caller's effort.
            for (connection, fallback) in [
                (&setup.native_id, "gpt-4o"),
                (
                    &setup.translated_id,
                    if provider == "bedrock" {
                        "us.anthropic.claude-future-6-v1:0"
                    } else {
                        "claude-future-6"
                    },
                ),
            ] {
                let alias = reasoning_route(&setup, &upstream, connection, fallback).await;
                for path in ["/v1/chat/completions", "/v1/responses"] {
                    for stream in [false, true] {
                        setup.recorder.calls.lock().await.clear();
                        setup.keys.usage.lock().await.clear();
                        let (status, headers, bytes) = send(
                            &setup,
                            path,
                            reasoning_body(path, &alias, Some(effort), stream).to_string(),
                            None,
                        )
                        .await;
                        assert_eq!(
                            status,
                            if path == "/v1/responses" && connection == &setup.native_id { 201 } else { 200 },
                            "{provider} {path} {stream}: {}",
                            String::from_utf8_lossy(&bytes)
                        );
                        assert_eq!(
                            headers.get("x-chalk-routed-model").and_then(|v| v.to_str().ok()),
                            Some(format!("{connection}/{fallback}").as_str()),
                            "{provider} {model} {path} {stream}: {headers:?}"
                        );
                        let calls = setup.recorder.calls.lock().await;
                        assert_eq!(calls.len(), 2, "one rejected attempt, one fallback");
                        let first: Value = serde_json::from_slice(&calls[0].2).unwrap();
                        let second: Value = serde_json::from_slice(&calls[1].2).unwrap();
                        assert_eq!(first["model"], upstream);
                        assert_eq!(second["model"], fallback);
                        if connection == &setup.native_id {
                            let sent_effort = if path == "/v1/responses" { &second["reasoning"]["effort"] } else { &second["reasoning_effort"] };
                            assert_eq!(sent_effort, effort);
                        } else {
                            assert_eq!(second.get("thinking"), first.get("thinking"));
                            assert_eq!(second.get("output_config"), first.get("output_config"));
                        }
                        assert_eq!(
                            setup.keys.usage.lock().await.len(),
                            1,
                            "usage only for successful generation"
                        );
                    }
                }
            }
            for path in ["/v1/chat/completions", "/v1/responses"] {
                for stream in [false, true] {
                    setup.recorder.calls.lock().await.clear();
                    let (status, headers, bytes) = send(
                        &setup,
                        path,
                        reasoning_body(path, &format!("translated/{upstream}"), Some(effort), stream)
                            .to_string(),
                        None,
                    )
                    .await;
                    assert_eq!(status, 400);
                    assert_eq!(headers["x-request-id"], "anthropic-validation");
                    assert_eq!(headers["retry-after"], "3");
                    let error: Value = serde_json::from_slice(&bytes).unwrap();
                    assert!(
                        error["error"]["message"].as_str().unwrap().contains(message),
                        "{error}"
                    );
                    assert_eq!(setup.recorder.calls.lock().await.len(), 1);
                }
            }
        }
    }
}

#[tokio::test]
async fn translated_reasoning_unrelated_or_ambiguous_rejections_stay_terminal() {
    let setup = boot(false, &[], None).await;
    let alias = reasoning_route(&setup, "claude-reject-fixture", &setup.native_id, "gpt-4o").await;
    for (status, message, effort) in [
        (
            400,
            "This model does not support tool_choice with adaptive thinking",
            Some("high"),
        ),
        (
            400,
            "adaptive thinking is not supported on this model",
            None,
        ),
        (
            422,
            "adaptive thinking is not supported on this model",
            Some("high"),
        ),
    ] {
        *setup.recorder.rejection.lock().await = Some((
            status,
            json!({"type":"error", "error":{"type":"invalid_request_error","message":message}}),
        ));
        for path in ["/v1/chat/completions", "/v1/responses"] {
            for stream in [false, true] {
                setup.recorder.calls.lock().await.clear();
                let body = reasoning_body(path, &alias, effort, stream);
                let (actual, headers, _) = send(&setup, path, body.to_string(), None).await;
                assert_eq!(actual.as_u16(), status, "{message} {path} {effort:?}");
                assert_eq!(headers["x-request-id"], "fixture-validation");
                assert_eq!(setup.recorder.calls.lock().await.len(), 1, "{message}");
            }
        }
    }
}

#[tokio::test]
async fn translated_reasoning_requires_a_valid_provider_error_envelope() {
    let setup = boot(false, &[], None).await;
    let alias = reasoning_route(&setup, "claude-reject-fixture", &setup.native_id, "gpt-4o").await;
    let envelope = json!({"type":"future-error","error":{"type":"invalid_request_error","message":"adaptive thinking is not supported on this model"}});
    *setup.recorder.rejection.lock().await = Some((400, envelope));
    for stream in [false, true] {
        setup.recorder.calls.lock().await.clear();
        let (status, _, _) = send(
            &setup,
            "/v1/chat/completions",
            reasoning_body("/v1/chat/completions", &alias, Some("high"), stream).to_string(),
            None,
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(setup.recorder.calls.lock().await.len(), 1);
    }
}

#[tokio::test]
async fn translated_reasoning_preserves_last_real_error_when_fallbacks_cannot_serve() {
    // A route's own fallbacks are covered by its grant, so none is skipped for
    // permission here; a skipped caller fallback is covered in `judge_test`.
    let setup = boot(false, &[], None).await;
    for (connection, fallback, expected_status, request_id) in [
        (
            &setup.translated_id,
            "claude-reject-disabled-alt",
            400,
            "anthropic-validation",
        ),
        (&setup.native_id, "reject503", 503, "error-503"),
    ] {
        let alias = reasoning_route(&setup, "claude-reject-disabled", connection, fallback).await;
        for path in ["/v1/chat/completions", "/v1/responses"] {
            for stream in [false, true] {
                setup.recorder.calls.lock().await.clear();
                let (status, headers, _) = send(
                    &setup,
                    path,
                    reasoning_body(path, &alias, Some("none"), stream).to_string(),
                    None,
                )
                .await;
                assert_eq!(status, expected_status);
                assert_eq!(headers["x-request-id"], request_id);
                assert_eq!(setup.recorder.calls.lock().await.len(), 2);
                assert!(setup.keys.usage.lock().await.is_empty());
            }
        }
    }
}

#[tokio::test]
async fn native_reasoning_validation_never_uses_translated_error_classification() {
    let setup = boot(false, &[], None).await;
    *setup.recorder.rejection.lock().await = Some((
        400,
        json!({"type":"error","error":{"type":"invalid_request_error","message":"adaptive thinking is not supported on this model"}}),
    ));
    let alias = reasoning_route_from(
        &setup,
        &setup.native_id,
        "reject-fixture",
        &setup.translated_id,
        "claude-future-6",
    )
    .await;
    let messages_alias = reasoning_route(
        &setup,
        "claude-reject-fixture",
        &setup.translated_id,
        "claude-future-6",
    )
    .await;
    for stream in [false, true] {
        for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
            setup.recorder.calls.lock().await.clear();
            let body = if path == "/v1/messages" {
                json!({"model":messages_alias,"max_tokens":64,"messages":[{"role":"user","content":"hello"}],"thinking":{"type":"adaptive"},"stream":stream})
            } else {
                reasoning_body(path, &alias, Some("high"), stream)
            };
            let (status, _, _) = send(&setup, path, body.to_string(), None).await;
            assert_eq!(status, 400);
            assert_eq!(setup.recorder.calls.lock().await.len(), 1);
        }
    }
}

#[tokio::test]
async fn translated_reasoning_error_after_stream_start_never_falls_back() {
    let setup = boot(false, &[], None).await;
    let alias = reasoning_route(&setup, "claude-late-error", &setup.native_id, "gpt-4o").await;
    for path in ["/v1/chat/completions", "/v1/responses"] {
        setup.recorder.calls.lock().await.clear();
        let (status, _, bytes) = send(
            &setup,
            path,
            reasoning_body(path, &alias, Some("high"), true).to_string(),
            None,
        )
        .await;
        assert_eq!(status, 200);
        assert!(!bytes.is_empty(), "the opened stream was returned");
        assert_eq!(setup.recorder.calls.lock().await.len(), 1);
    }
}

#[tokio::test]
async fn translated_reasoning_rejects_malformed_effort_before_transport() {
    for provider in ["anthropic", "bedrock"] {
        let setup = boot_with_translated_provider(false, &[], None, provider).await;
        let model = if provider == "bedrock" {
            "translated/us.anthropic.claude-future-6-v1:0"
        } else {
            "translated/claude-future-6"
        };
        for path in ["/v1/chat/completions", "/v1/responses"] {
            for stream in [false, true] {
                let mut body = reasoning_body(path, model, Some("high"), stream);
                if path == "/v1/responses" {
                    body["reasoning"]["effort"] = json!(42);
                } else {
                    body["reasoning_effort"] = json!(42);
                }
                let (status, _, bytes) = send(&setup, path, body.to_string(), None).await;
                assert_eq!(
                    status,
                    400,
                    "{provider} {path} {stream}: {}",
                    String::from_utf8_lossy(&bytes)
                );
                assert!(setup.recorder.calls.lock().await.is_empty());
            }
        }
    }
}

#[tokio::test]
async fn native_success_status_and_mime_survive_without_becoming_a_changed_cache_reply() {
    let setup = boot(true, &[], None).await;
    for stream in [false, true] {
        for _ in 0..2 {
            let (status, headers, bytes) = send(
                &setup,
                "/v1/chat/completions",
                chat("native/created", stream),
                None,
            )
            .await;
            assert_eq!(status, 201);
            assert_eq!(
                headers["content-type"],
                if stream {
                    "text/event-stream; charset=utf-8"
                } else {
                    "application/json; charset=utf-8"
                }
            );
            if stream {
                assert!(std::str::from_utf8(&bytes).unwrap().contains("hi there"));
            } else {
                assert_eq!(bytes, RICH_REPLY);
            }
        }
    }
    assert_eq!(setup.cache.len(), 0);
    assert_eq!(setup.recorder.calls.lock().await.len(), 4);
}

async fn response_operation(
    setup: &Setup,
    method: &str,
    path: &str,
) -> (StatusCode, HeaderMap, Bytes) {
    let response = setup
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {}", setup.key))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, bytes)
}

fn wrapped_response(setup: &Setup, wire: &str) -> String {
    let locator = setup
        .state
        .response_locator
        .encode(&setup.native_id, None, "resp-native");
    // Expected bytes are constructed without parsing the fixture: every byte
    // except the response ID string token must survive.
    wire.replace("\"resp-native\"", &format!("\"{locator}\""))
}

#[tokio::test]
async fn native_responses_json_operations_preserve_wire_metadata_and_locator_round_trips() {
    let setup = boot(false, &[], None).await;
    let (status, headers, bytes) = send(
        &setup,
        "/v1/responses",
        r#"{"model":"native/gpt-4o","input":"hello"}"#,
        None,
    )
    .await;
    assert_eq!(status, 201);
    assert_eq!(headers["content-type"], "application/json; charset=utf-8");
    assert_headers(&headers, "responses-json", "3");
    assert_eq!(bytes, wrapped_response(&setup, RESPONSES_REPLY));
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    let id = value["id"].as_str().unwrap();
    let locator = setup.state.response_locator.decode(id).unwrap();
    assert_eq!(locator.connection_id, setup.native_id);
    assert_eq!(locator.upstream_response_id, "resp-native");

    let (status, headers, bytes) =
        response_operation(&setup, "GET", &format!("/v1/responses/{id}")).await;
    assert_eq!(status, 201);
    assert_eq!(headers["content-type"], "application/json; charset=utf-8");
    assert_eq!(bytes, wrapped_response(&setup, RESPONSES_REPLY));
    let (status, headers, bytes) =
        response_operation(&setup, "POST", &format!("/v1/responses/{id}/cancel")).await;
    assert_eq!(status, 202);
    assert_headers(&headers, "responses-cancel", "3");
    assert_eq!(
        bytes,
        wrapped_response(&setup, &RESPONSES_REPLY.replace("completed", "cancelled"))
    );
    let (status, headers, bytes) =
        response_operation(&setup, "DELETE", &format!("/v1/responses/{id}")).await;
    assert_eq!(status, 202);
    assert_headers(&headers, "responses-delete", "3");
    assert_eq!(bytes, wrapped_response(&setup, DELETED_REPLY));

    let (status, _, _) = send(
        &setup,
        "/v1/responses",
        json!({
            "model":"native/gpt-4o", "input":"next", "previous_response_id":id,
        })
        .to_string(),
        None,
    )
    .await;
    assert_eq!(status, 201);
    let calls = setup.recorder.calls.lock().await;
    let request: Value = serde_json::from_slice(&calls.last().unwrap().2).unwrap();
    assert_eq!(request["previous_response_id"], "resp-native");
    assert_eq!(calls[1].0, "/responses/resp-native");
    assert_eq!(calls[2].0, "/responses/resp-native/cancel");
    assert_eq!(calls[3].0, "/responses/resp-native");
    drop(calls);
    assert_eq!(*setup.keys.usage.lock().await, vec![(7, 3, 10); 2]);

    let (status, headers, bytes) = send(
        &setup,
        "/v1/responses/compact",
        r#"{"model":"native/gpt-4o","input":[]}"#,
        None,
    )
    .await;
    assert_eq!(status, 201);
    assert_eq!(headers["content-type"], "application/json; charset=utf-8");
    assert_eq!(
        bytes, RESPONSES_REPLY,
        "compaction has no locator transformation"
    );

    let empty_id = setup
        .state
        .response_locator
        .encode(&setup.native_id, None, "resp-empty");
    let (status, headers, bytes) =
        response_operation(&setup, "DELETE", &format!("/v1/responses/{empty_id}")).await;
    assert_eq!(status, 204);
    assert_headers(&headers, "responses-empty", "3");
    assert!(
        bytes.is_empty(),
        "a bodyless native success must not become JSON null"
    );
}

#[tokio::test]
async fn native_responses_sse_preserves_payload_and_framing_through_locator_rewrites() {
    for background in [false, true] {
        let setup = boot(false, &[], None).await;
        let (status, headers, bytes) = send(
            &setup,
            "/v1/responses",
            json!({
                "model":"native/gpt-4o", "input":"hello", "stream":true, "background":background,
            })
            .to_string(),
            None,
        )
        .await;
        assert_eq!(
            status,
            201,
            "background={background}: {}",
            String::from_utf8_lossy(&bytes)
        );
        assert_eq!(headers["content-type"], "text/event-stream; charset=utf-8");
        assert_headers(&headers, "responses-stream", "3");
        assert_eq!(
            bytes,
            wrapped_response(&setup, &responses_sse()),
            "background={background}"
        );
        assert_eq!(*setup.keys.usage.lock().await, vec![(7, 3, 10)]);

        let id = setup
            .state
            .response_locator
            .encode(&setup.native_id, None, "resp-native");
        for _ in 0..2 {
            let (status, headers, bytes) = response_operation(
                &setup,
                "GET",
                &format!("/v1/responses/{id}?stream=true&starting_after=42"),
            )
            .await;
            assert_eq!(status, 201);
            assert_eq!(headers["content-type"], "text/event-stream; charset=utf-8");
            assert_eq!(bytes, wrapped_response(&setup, &responses_sse()));
        }
        assert!(setup
            .recorder
            .calls
            .lock()
            .await
            .last()
            .unwrap()
            .0
            .contains("starting_after=42"));
        setup
            .state
            .background_responses
            .reconcile(&setup.state)
            .await
            .unwrap();
        assert_eq!(
            *setup.keys.usage.lock().await,
            vec![(7, 3, 10)],
            "replayed terminal events must not charge twice"
        );
    }
}

#[tokio::test]
async fn native_responses_malformed_json_and_wrong_sse_mime_fail_before_success() {
    let setup = boot(false, &[], None).await;
    for (model, stream) in [("malformed", false), ("bad-mime", true)] {
        let (status, headers, bytes) = send(
            &setup,
            "/v1/responses",
            json!({
                "model":format!("native/{model}"), "input":"hello", "stream":stream,
            })
            .to_string(),
            None,
        )
        .await;
        assert_eq!(status, 502, "{}", String::from_utf8_lossy(&bytes));
        assert_eq!(headers["content-type"], "application/json");
        assert!(serde_json::from_slice::<Value>(&bytes).unwrap()["error"].is_object());
    }
    assert!(setup.keys.usage.lock().await.is_empty());
    assert_eq!(setup.recorder.calls.lock().await.len(), 2);
}

#[tokio::test]
async fn native_responses_fallback_preserves_winner_and_does_not_retry_invalid_successes() {
    let setup = boot(false, &["native/gpt-4o"], None).await;
    for stream in [false, true] {
        let (status, headers, bytes) = send(
            &setup,
            "/v1/responses",
            json!({
                "model":"native/reject503", "input":"hello", "stream":stream,
            })
            .to_string(),
            None,
        )
        .await;
        assert_eq!(status, 201);
        assert_eq!(
            headers["content-type"],
            if stream {
                "text/event-stream; charset=utf-8"
            } else {
                "application/json; charset=utf-8"
            }
        );
        assert_eq!(
            bytes,
            wrapped_response(
                &setup,
                &if stream {
                    responses_sse()
                } else {
                    RESPONSES_REPLY.to_owned()
                }
            )
        );
    }
    assert_eq!(setup.recorder.calls.lock().await.len(), 4);
    assert_eq!(*setup.keys.usage.lock().await, vec![(7, 3, 10); 2]);
    for (model, stream, expected) in [
        ("reject400", false, 400),
        ("reject400", true, 400),
        ("malformed", false, 502),
        ("bad-mime", true, 502),
    ] {
        let before = setup.recorder.calls.lock().await.len();
        let (status, _, _) = send(
            &setup,
            "/v1/responses",
            json!({
                "model":format!("native/{model}"), "input":"hello", "stream":stream,
            })
            .to_string(),
            None,
        )
        .await;
        assert_eq!(status, expected);
        assert_eq!(
            setup.recorder.calls.lock().await.len(),
            before + 1,
            "{model} must not dispatch a fallback generation"
        );
    }
}

#[tokio::test]
async fn native_responses_permissions_gate_json_and_sse_before_upstream() {
    let setup = boot(
        false,
        &[],
        Some(ApiKeyPermission {
            provider: Some("anthropic".into()),
            ..Default::default()
        }),
    )
    .await;
    for stream in [false, true] {
        let (status, _, _) = send(
            &setup,
            "/v1/responses",
            json!({
                "model":"native/gpt-4o", "input":"hello", "stream":stream,
            })
            .to_string(),
            None,
        )
        .await;
        assert_eq!(status, 403);
    }
    assert!(setup.recorder.calls.lock().await.is_empty());
    assert!(setup.keys.usage.lock().await.is_empty());
}
