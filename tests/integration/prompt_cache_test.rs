//! Exact-prefix completion caching (`openai_server::prompt_cache`).
//!
//! These drive the real chat handler behind the real auth middleware with a
//! mock backend that counts dispatches, so "was this served from cache?" is
//! answered by whether the upstream was called at all — not just by a header.
//!
//! What they pin down:
//!   * an identical prompt is answered without a second upstream call, in both
//!     the streaming and non-streaming shapes, and across the two,
//!   * a key that did not opt in is never cached,
//!   * the scope decides who may share an entry, and
//!   * the cache is not a way around the permission gate, nor a way to serve a
//!     generation that never finished.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::{middleware, routing::post, Router};

use openai_server::{
    api_keys::{ApiKey, ApiKeyPermission, ApiKeyStore, InMemoryApiKeyStore},
    backend::{BackendError, BackendProvider, ResolvedRoute, RouteResolution, SseByteStream},
    hashing::KeyHasher,
    middleware::auth_middleware,
    prompt_cache::{memory::MemoryPromptCache, CachePolicy, CacheScope},
    quota::MemoryQuota,
    ratelimit::{MemoryRateLimitPolicyStore, RateLimitPolicy, RateLimitPolicyStore},
    types::{
        ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatMessage,
        CreateEmbeddingRequest, CreateEmbeddingResponse, CreateImageRequest,
        CreateModerationRequest, CreateModerationResponse, CreateSpeechRequest, ImageResponse,
        ListResponse, Model, TranscriptionResponse, Usage,
    },
};

use crate::support::app::AppStateBuilder;

/// A model whose stream stops generating without a `finish_reason`, standing in
/// for a truncated upstream.
const TRUNCATED_MODEL: &str = "openai/truncated";

// ---------------------------------------------------------------------------
// Mock backend
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct MockBackend {
    /// Every dispatch (streaming or not), in order. A cache hit must not add
    /// to this.
    calls: Arc<Mutex<Vec<String>>>,
}

impl MockBackend {
    fn resolved_route(&self, model: &str) -> ResolvedRoute {
        let kind = model
            .split_once('/')
            .map(|(k, _)| k)
            .unwrap_or(model)
            .to_string();
        ResolvedRoute {
            routing: Default::default(),
            backend: Arc::new(self.clone()),
            stripped_model: model.to_string(),
            capabilities: Arc::new(
                openai_server::capabilities::resolve("openai-compatible", model, None, None)
                    .unwrap(),
            ),
            resolution: RouteResolution {
                provider_kind: kind.clone(),
                connection_id: None,
                connection_name: kind,
                upstream_model_id: model.to_string(),
                connection_prefix: None,
                model_route_id: None,
            },
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

/// A complete SSE response whose content names the model that served it.
fn sse_for(model: &str) -> SseByteStream {
    let chunks = vec![
        format!("data: {{\"id\":\"chatcmpl-pc\",\"object\":\"chat.completion.chunk\",\"created\":7,\"model\":\"{model}\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\"}},\"finish_reason\":null}}]}}\n\n"),
        format!("data: {{\"id\":\"chatcmpl-pc\",\"object\":\"chat.completion.chunk\",\"created\":7,\"model\":\"{model}\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"served-by:{model}\"}},\"finish_reason\":null}}]}}\n\n"),
        format!("data: {{\"id\":\"chatcmpl-pc\",\"object\":\"chat.completion.chunk\",\"created\":7,\"model\":\"{model}\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":3,\"completion_tokens\":4,\"total_tokens\":7}}}}\n\n"),
        "data: [DONE]\n\n".to_string(),
    ];
    Box::pin(futures::stream::iter(
        chunks
            .into_iter()
            .map(|c| Ok(bytes::Bytes::from(c)))
            .collect::<Vec<_>>(),
    ))
}

/// A stream that ends cleanly but mid-generation: content, then end of stream,
/// with no `finish_reason` and no `[DONE]`.
fn truncated_sse(model: &str) -> SseByteStream {
    let chunks = vec![
        format!("data: {{\"id\":\"chatcmpl-pc\",\"object\":\"chat.completion.chunk\",\"created\":7,\"model\":\"{model}\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\"}},\"finish_reason\":null}}]}}\n\n"),
        format!("data: {{\"id\":\"chatcmpl-pc\",\"object\":\"chat.completion.chunk\",\"created\":7,\"model\":\"{model}\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"half an ans\"}},\"finish_reason\":null}}]}}\n\n"),
    ];
    Box::pin(futures::stream::iter(
        chunks
            .into_iter()
            .map(|c| Ok(bytes::Bytes::from(c)))
            .collect::<Vec<_>>(),
    ))
}

fn fixed_response(model: &str) -> ChatCompletionResponse {
    ChatCompletionResponse {
        id: "chatcmpl-pc".to_string(),
        object: "chat.completion".to_string(),
        created: 7,
        model: model.to_string(),
        choices: vec![ChatChoice {
            index: 0,
            message: ChatMessage {
                role: "assistant".to_string(),
                content: Some(serde_json::Value::String(format!("served-by:{model}"))),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            },
            finish_reason: Some("stop".to_string()),
        }],
        usage: Some(Usage {
            prompt_tokens: 3,
            completion_tokens: Some(4),
            total_tokens: 7,
            ..Default::default()
        }),
    }
}

#[async_trait]
impl BackendProvider for MockBackend {
    async fn create_chat_completion_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<SseByteStream, BackendError> {
        self.calls.lock().unwrap().push(request.model.clone());
        Ok(if request.model == TRUNCATED_MODEL {
            truncated_sse(&request.model)
        } else {
            sse_for(&request.model)
        })
    }

    async fn create_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        self.calls.lock().unwrap().push(request.model.clone());
        Ok(fixed_response(&request.model))
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
    #[allow(clippy::too_many_arguments)]
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
    async fn resolve(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        Ok(self.resolved_route(model))
    }
    async fn resolve_strict(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        Ok(self.resolved_route(model))
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Setup {
    backend: Arc<MockBackend>,
    cache: Arc<MemoryPromptCache>,
    port: u16,
}

/// One seeded API key: its raw secret and the caching policy it carries.
struct KeySpec {
    raw: &'static str,
    permission: ApiKeyPermission,
    usage_pool_id: Option<&'static str>,
}

fn cached_key(raw: &'static str, scope: CacheScope) -> KeySpec {
    KeySpec {
        raw,
        permission: ApiKeyPermission {
            cache: Some(CachePolicy::new(scope)),
            ..ApiKeyPermission::default()
        },
        usage_pool_id: None,
    }
}

async fn boot(keys: Vec<KeySpec>) -> Setup {
    boot_with_global_rpm(keys, None).await
}

async fn boot_with_global_rpm(keys: Vec<KeySpec>, limit: Option<(String, u64)>) -> Setup {
    let backend = Arc::new(MockBackend::default());
    let cache = Arc::new(MemoryPromptCache::new(64, 8 << 20));
    let hasher = KeyHasher::new(b"prompt-cache-test-pepper".to_vec());
    let store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(hasher.clone()));
    for spec in keys {
        store
            .insert_raw(ApiKey {
                key_hash: hasher.hash(spec.raw),
                key_prefix: openai_server::hashing::key_prefix(spec.raw),
                description: "prompt cache test key".to_string(),
                created_at: chrono::Utc::now(),
                permission: spec.permission,
                labels: HashMap::new(),
                usage_pool_id: spec.usage_pool_id.map(str::to_string),
                daily_token_budget: None,
            })
            .await
            .expect("seed key");
    }

    // No test in this file sends a Chalk JWT (auth is always `Bearer {key}`
    // against the seeded api-key store), so the builder's default empty
    // federated-auth registry is equivalent to the previous explicit
    // `OidcRegistry::from_config(&[])` and needs no override.
    let mut state = AppStateBuilder::new(backend.clone())
        .api_key_store(store)
        .prompt_cache(cache.clone())
        .build()
        .await;
    if let Some((raw_key, burst)) = limit {
        let rate_store = Arc::new(MemoryRateLimitPolicyStore::new("test".to_string()));
        rate_store
            .create(RateLimitPolicy {
                quota_policy_id: "cache-rpm".to_string(),
                id: "cache-rpm-source".to_string(),
                limit_type: "requests_per_minute".to_string(),
                limit_value: 1,
                burst_value: Some(burst),
                connection_id: None,
                models: Vec::new(),
                scope: "api_key".to_string(),
                scope_id: hasher.hash(&raw_key),
                enabled: true,
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
        let budget_store = openai_server::budget::from_core_store(None, "test".to_string());
        state.budget_store = budget_store.clone();
        state.rate_limit_store = rate_store.clone();
        state.quota_resolver = Arc::new(openai_server::quota_policy::QuotaPolicyResolver::new(
            "test".to_string(),
            budget_store,
            rate_store,
        ));
        state.quota_backend = Arc::new(MemoryQuota::new());
    }

    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(openai_server::handlers::chat::create_chat_completion),
        )
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, auth_middleware));

    let base = crate::support::mock::serve(app).await;
    let port = base.rsplit(':').next().unwrap().parse().unwrap();
    Setup {
        backend,
        cache,
        port,
    }
}

/// Writes are spawned off the request path, so a test that has just read a
/// response has no guarantee the entry has landed yet. Wait for the count the
/// test expects rather than sleeping a fixed amount.
async fn await_entries(setup: &Setup, expected: usize) {
    for _ in 0..200 {
        if setup.cache.len() >= expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "cache never reached {expected} entries (stuck at {})",
        setup.cache.len()
    );
}

async fn send_chat(
    port: u16,
    key: &str,
    model: &str,
    prompt: &str,
    stream: bool,
) -> reqwest::Response {
    let mut body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
    });
    if stream {
        body["stream"] = serde_json::Value::Bool(true);
    }
    reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .expect("send")
}

fn cache_status(res: &reqwest::Response) -> Option<String> {
    res.headers()
        .get("x-chalk-cache")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Assemble the assistant text from an SSE body, so a replayed cache hit and a
/// live stream can be compared on content rather than on chunk boundaries.
fn sse_content(body: &str) -> String {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        .filter_map(|chunk| {
            chunk["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_string)
        })
        .collect()
}

const KEY: &str = "sk-prompt-cache-test";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn identical_non_streaming_prompts_hit_without_a_second_upstream_call() {
    let setup = boot(vec![cached_key(KEY, CacheScope::Token)]).await;

    let first = send_chat(setup.port, KEY, "openai/gpt-4o", "what is 2+2?", false).await;
    assert_eq!(cache_status(&first).as_deref(), Some("miss"));
    let first_body = first.text().await.unwrap();
    await_entries(&setup, 1).await;

    let second = send_chat(setup.port, KEY, "openai/gpt-4o", "what is 2+2?", false).await;
    assert_eq!(cache_status(&second).as_deref(), Some("hit"));
    assert!(
        second.headers().contains_key("x-chalk-cache-age"),
        "a hit reports how old the entry is"
    );
    let second_body = second.text().await.unwrap();

    assert_eq!(
        second_body, first_body,
        "a hit is byte-identical to the live answer"
    );
    assert_eq!(
        setup.backend.calls().len(),
        1,
        "the second request must not reach the upstream: {:?}",
        setup.backend.calls()
    );
}

#[tokio::test]
async fn cache_hits_consume_global_request_rate_without_an_upstream_dispatch() {
    let setup = boot_with_global_rpm(
        vec![cached_key(KEY, CacheScope::Token)],
        Some((KEY.to_string(), 2)),
    )
    .await;

    let first = send_chat(setup.port, KEY, "openai/gpt-4o", "cached rate", false).await;
    assert_eq!(first.status(), 200);
    let _ = first.text().await;
    await_entries(&setup, 1).await;

    let hit = send_chat(setup.port, KEY, "openai/gpt-4o", "cached rate", false).await;
    assert_eq!(hit.status(), 200);
    assert_eq!(cache_status(&hit).as_deref(), Some("hit"));
    let _ = hit.text().await;

    let limited = send_chat(setup.port, KEY, "openai/gpt-4o", "cached rate", false).await;
    assert_eq!(limited.status(), 429);
    assert_eq!(setup.backend.calls().len(), 1);
}

#[tokio::test]
async fn identical_streaming_prompts_hit_and_replay_as_sse() {
    let setup = boot(vec![cached_key(KEY, CacheScope::Token)]).await;

    let first = send_chat(setup.port, KEY, "openai/gpt-4o", "stream me", true).await;
    assert_eq!(cache_status(&first).as_deref(), Some("miss"));
    let first_body = first.text().await.unwrap();
    // The entry is written when the tap sees the stream end, which is only
    // once the body above has been fully read.
    await_entries(&setup, 1).await;

    let second = send_chat(setup.port, KEY, "openai/gpt-4o", "stream me", true).await;
    assert_eq!(cache_status(&second).as_deref(), Some("hit"));
    assert_eq!(
        second.headers().get(reqwest::header::CONTENT_TYPE).unwrap(),
        "text/event-stream",
        "a streaming hit is still a stream"
    );
    let second_body = second.text().await.unwrap();

    assert_eq!(sse_content(&second_body), sse_content(&first_body));
    assert!(
        second_body.ends_with("data: [DONE]\n\n"),
        "replay terminates the stream"
    );
    assert_eq!(
        setup.backend.calls().len(),
        1,
        "no second upstream stream was opened"
    );
}

#[tokio::test]
async fn a_streamed_miss_serves_a_non_streamed_hit_and_the_reverse() {
    let setup = boot(vec![cached_key(KEY, CacheScope::Token)]).await;

    // Streamed first, then asked for non-streamed: one entry, one upstream call.
    let streamed = send_chat(setup.port, KEY, "openai/gpt-4o", "shared prompt", true).await;
    let streamed_body = streamed.text().await.unwrap();
    await_entries(&setup, 1).await;

    let non_streamed = send_chat(setup.port, KEY, "openai/gpt-4o", "shared prompt", false).await;
    assert_eq!(cache_status(&non_streamed).as_deref(), Some("hit"));
    let parsed: serde_json::Value = non_streamed.json().await.unwrap();
    assert_eq!(
        parsed["choices"][0]["message"]["content"].as_str().unwrap(),
        sse_content(&streamed_body),
        "the non-streamed hit carries the content the stream produced"
    );

    // ...and the other direction, from a non-streamed miss.
    let fresh = send_chat(setup.port, KEY, "openai/gpt-4o", "other prompt", false).await;
    assert_eq!(cache_status(&fresh).as_deref(), Some("miss"));
    let _ = fresh.text().await.unwrap();
    await_entries(&setup, 2).await;

    let replayed = send_chat(setup.port, KEY, "openai/gpt-4o", "other prompt", true).await;
    assert_eq!(cache_status(&replayed).as_deref(), Some("hit"));
    assert_eq!(
        sse_content(&replayed.text().await.unwrap()),
        "served-by:openai/gpt-4o"
    );

    assert_eq!(
        setup.backend.calls().len(),
        2,
        "exactly one upstream call per distinct prompt: {:?}",
        setup.backend.calls()
    );
}

#[tokio::test]
async fn a_different_prefix_misses() {
    let setup = boot(vec![cached_key(KEY, CacheScope::Token)]).await;

    let _ = send_chat(setup.port, KEY, "openai/gpt-4o", "first question", false)
        .await
        .text()
        .await;
    await_entries(&setup, 1).await;

    let other = send_chat(setup.port, KEY, "openai/gpt-4o", "second question", false).await;
    assert_eq!(cache_status(&other).as_deref(), Some("miss"));
    let _ = other.text().await;

    // Same prompt, different model: also a miss, since the model is part of the
    // digest at every scope.
    let other_model = send_chat(
        setup.port,
        KEY,
        "openai/gpt-4o-mini",
        "first question",
        false,
    )
    .await;
    assert_eq!(cache_status(&other_model).as_deref(), Some("miss"));

    assert_eq!(setup.backend.calls().len(), 3);
}

#[tokio::test]
async fn a_key_that_did_not_opt_in_is_never_cached() {
    let setup = boot(vec![KeySpec {
        raw: KEY,
        permission: ApiKeyPermission::default(),
        usage_pool_id: None,
    }])
    .await;

    let first = send_chat(setup.port, KEY, "openai/gpt-4o", "hello", false).await;
    assert_eq!(
        cache_status(&first),
        None,
        "no cache header at all for a key with no policy"
    );
    let _ = first.text().await;

    let second = send_chat(setup.port, KEY, "openai/gpt-4o", "hello", false).await;
    assert_eq!(cache_status(&second), None);
    let _ = second.text().await;

    assert_eq!(
        setup.backend.calls().len(),
        2,
        "both requests reached the upstream"
    );
    assert_eq!(setup.cache.len(), 0, "nothing was written");
}

#[tokio::test]
async fn a_disabled_policy_is_not_cached() {
    let setup = boot(vec![KeySpec {
        raw: KEY,
        permission: ApiKeyPermission {
            cache: Some(CachePolicy {
                enabled: false,
                ..CachePolicy::new(CacheScope::Token)
            }),
            ..ApiKeyPermission::default()
        },
        usage_pool_id: None,
    }])
    .await;

    let _ = send_chat(setup.port, KEY, "openai/gpt-4o", "hello", false)
        .await
        .text()
        .await;
    let second = send_chat(setup.port, KEY, "openai/gpt-4o", "hello", false).await;
    assert_eq!(cache_status(&second), None);
    assert_eq!(setup.backend.calls().len(), 2);
}

#[tokio::test]
async fn token_scope_does_not_leak_between_keys() {
    const OTHER: &str = "sk-prompt-cache-other";
    let setup = boot(vec![
        cached_key(KEY, CacheScope::Token),
        cached_key(OTHER, CacheScope::Token),
    ])
    .await;

    let _ = send_chat(setup.port, KEY, "openai/gpt-4o", "private", false)
        .await
        .text()
        .await;
    await_entries(&setup, 1).await;

    let other = send_chat(setup.port, OTHER, "openai/gpt-4o", "private", false).await;
    assert_eq!(
        cache_status(&other).as_deref(),
        Some("miss"),
        "another token must not be served this key's completion"
    );
    assert_eq!(setup.backend.calls().len(), 2);
}

#[tokio::test]
async fn model_scope_shares_between_keys() {
    const OTHER: &str = "sk-prompt-cache-other";
    let setup = boot(vec![
        cached_key(KEY, CacheScope::Model),
        cached_key(OTHER, CacheScope::Model),
    ])
    .await;

    let _ = send_chat(setup.port, KEY, "openai/gpt-4o", "shared", false)
        .await
        .text()
        .await;
    await_entries(&setup, 1).await;

    let other = send_chat(setup.port, OTHER, "openai/gpt-4o", "shared", false).await;
    assert_eq!(
        cache_status(&other).as_deref(),
        Some("hit"),
        "model scope is environment-wide by design"
    );
    assert_eq!(setup.backend.calls().len(), 1);
}

#[tokio::test]
async fn a_cache_hit_does_not_bypass_the_permission_gate() {
    const RESTRICTED: &str = "sk-prompt-cache-restricted";
    let setup = boot(vec![
        cached_key(KEY, CacheScope::Model),
        KeySpec {
            raw: RESTRICTED,
            // Shares the cache key space (model scope), but may only reach
            // anthropic — so the openai entry below must stay out of reach.
            permission: ApiKeyPermission {
                provider: Some("anthropic".to_string()),
                cache: Some(CachePolicy::new(CacheScope::Model)),
                ..ApiKeyPermission::default()
            },
            usage_pool_id: None,
        },
    ])
    .await;

    let _ = send_chat(setup.port, KEY, "openai/gpt-4o", "secret", false)
        .await
        .text()
        .await;
    await_entries(&setup, 1).await;

    let denied = send_chat(setup.port, RESTRICTED, "openai/gpt-4o", "secret", false).await;
    assert_eq!(
        denied.status(),
        reqwest::StatusCode::FORBIDDEN,
        "the allowlist is enforced before the cache is consulted"
    );
    let body = denied.text().await.unwrap();
    assert!(
        !body.contains("served-by"),
        "no cached content leaked in the denial"
    );
}

#[tokio::test]
async fn a_truncated_stream_is_not_cached() {
    let setup = boot(vec![cached_key(KEY, CacheScope::Token)]).await;

    let first = send_chat(setup.port, KEY, TRUNCATED_MODEL, "cut me off", true).await;
    let body = first.text().await.unwrap();
    assert!(
        body.contains("half an ans"),
        "the partial generation still reached the client"
    );

    // The stream must be fully drained for `.text()` to return, and the
    // cache-or-not decision happens synchronously inline when the stream
    // ends — no spawned write is ever reached for a truncated generation, so
    // there is nothing to wait out.
    assert_eq!(
        setup.cache.len(),
        0,
        "an unfinished generation must never be cached"
    );

    let second = send_chat(setup.port, KEY, TRUNCATED_MODEL, "cut me off", true).await;
    assert_eq!(cache_status(&second).as_deref(), Some("miss"));
    assert_eq!(setup.backend.calls().len(), 2);
}

#[tokio::test]
async fn a_streamed_response_over_the_entry_ceiling_is_not_cached() {
    // A one-byte ceiling: any non-empty generation exceeds it, so this pins
    // down the same guarantee `a_truncated_stream_is_not_cached` gives a
    // clean-but-oversized stream instead of an unfinished one.
    let setup = boot(vec![KeySpec {
        raw: KEY,
        permission: ApiKeyPermission {
            cache: Some(CachePolicy {
                max_entry_bytes: Some(1),
                ..CachePolicy::new(CacheScope::Token)
            }),
            ..ApiKeyPermission::default()
        },
        usage_pool_id: None,
    }])
    .await;

    let first = send_chat(setup.port, KEY, "openai/gpt-4o", "too big to cache", true).await;
    let body = first.text().await.unwrap();
    assert_eq!(
        sse_content(&body),
        "served-by:openai/gpt-4o",
        "the full generation still reaches the client despite the ceiling"
    );

    // The stream must be fully drained for `.text()` to return, and the
    // cache-or-not decision happens synchronously inline when the stream
    // ends — the entry-ceiling check rejects the write before any spawn, so
    // there is nothing to wait out.
    assert_eq!(
        setup.cache.len(),
        0,
        "an entry over the ceiling must never be cached"
    );

    let second = send_chat(setup.port, KEY, "openai/gpt-4o", "too big to cache", true).await;
    assert_eq!(cache_status(&second).as_deref(), Some("miss"));
    assert_eq!(
        setup.backend.calls().len(),
        2,
        "no entry was cached, so the second request reaches the upstream too"
    );
}
