//! RFP-0002: streaming fallback (retry-before-first-byte).
//!
//! The fallback policy now applies to streaming chat, in the window *before* the
//! router commits its `200 OK`. Every adapter checks upstream status before
//! handing back the byte stream, so a candidate whose stream fails to *open* has
//! committed nothing and leaked no bytes — the retry is invisible to the client.
//! Once bytes are flowing we are past the point of no return (mid-stream failover
//! is an explicit non-goal).
//!
//! These drive the real chat handler behind the auth middleware, with a mock
//! backend that overrides `create_chat_completion_stream` so a test can steer
//! which candidate's *open* succeeds and observe exactly which candidates were
//! attempted.
//!
//! Model-name conventions understood by the mock:
//!   * `…fail…`         — open fails with a 502
//!   * `…dispatch-NNN…` — open fails with upstream status `NNN`
//!   * anything else — open succeeds, streaming SSE that names the served model

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::{middleware, routing::post, Router};
use futures::StreamExt;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::export::trace::SpanData;
use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
use opentelemetry_sdk::trace::{SimpleSpanProcessor, TracerProvider};
use tower::ServiceExt;
use tracing_subscriber::layer::SubscriberExt;

use openai_server::{
    api_keys::{ApiKeyPermission, ApiKeyStore, InMemoryApiKeyStore},
    backend::{BackendError, BackendProvider, ResolvedRoute, RouteResolution, SseByteStream},
    fallback::FallbackPolicy,
    hashing::KeyHasher,
    middleware::auth_middleware,
    quota::{
        MemoryQuota, QuotaAction, QuotaAlgorithm, QuotaBackend, QuotaBackendError, QuotaDecision,
        QuotaGrant, QuotaMeter, QuotaOutcome, QuotaSource,
    },
    ratelimit::{MemoryRateLimitPolicyStore, RateLimitPolicy, RateLimitPolicyStore},
    types::{
        ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatMessage,
        CreateEmbeddingRequest, CreateEmbeddingResponse, CreateImageRequest,
        CreateModerationRequest, CreateModerationResponse, CreateSpeechRequest, ImageResponse,
        ListResponse, Model, TranscriptionResponse, Usage,
    },
    AppState,
};

use crate::support::app::AppStateBuilder;

const TEST_KEY: &str = "sk-streaming-fallback-test";

/// Split a model id into `(provider_kind, upstream_model_id)` on the first `/`.
fn split_model(model: &str) -> (String, String) {
    match model.split_once('/') {
        Some((kind, rest)) => (kind.to_string(), rest.to_string()),
        None => (model.to_string(), model.to_string()),
    }
}

/// Mock backend that records every attempted stream-*open* (and every
/// non-streaming dispatch) so a test can assert exactly which candidates were
/// tried, in order, and that a skipped candidate was never dispatched.
#[derive(Clone, Default)]
struct MockBackend {
    /// Models whose stream-open was attempted, in order.
    open_calls: Arc<Mutex<Vec<String>>>,
    /// Models dispatched non-streaming, in order.
    chat_calls: Arc<Mutex<Vec<String>>>,
}

impl MockBackend {
    fn resolution(model: &str) -> RouteResolution {
        let (kind, _rest) = split_model(model);
        RouteResolution {
            provider_kind: kind.clone(),
            connection_id: Some(kind.clone()),
            connection_name: kind,
            // This fake does not strip prefixes: it dispatches (and attributes)
            // the full public model id, which is what these tests assert on.
            upstream_model_id: model.to_string(),
            connection_prefix: None,
            model_route_id: None,
        }
    }

    fn resolved_route(&self, model: &str) -> ResolvedRoute {
        let max_output_tokens = if model.contains("cap-64") {
            Some(64)
        } else if model.contains("cap-1024") {
            Some(1_024)
        } else {
            None
        };
        let declaration = (model.contains("no-tools") || max_output_tokens.is_some()).then(|| {
            let mut defaults = serde_json::Map::new();
            if model.contains("no-tools") {
                defaults.insert(
                    "features".to_string(),
                    serde_json::json!({"tool_calling": "unsupported"}),
                );
            }
            if let Some(max_output_tokens) = max_output_tokens {
                defaults.insert(
                    "limits".to_string(),
                    serde_json::json!({"max_output_tokens": max_output_tokens}),
                );
            }
            serde_json::from_value(serde_json::json!({
                "schema_version": 1,
                "model_defaults": defaults,
            }))
            .unwrap()
        });
        ResolvedRoute {
            routing: Default::default(),
            backend: Arc::new(self.clone()),
            stripped_model: model.to_string(),
            capabilities: Arc::new(
                openai_server::capabilities::resolve(
                    "openai-compatible",
                    model,
                    None,
                    declaration.as_ref(),
                )
                .unwrap(),
            ),
            resolution: Self::resolution(model),
        }
    }

    fn opened(&self) -> Vec<String> {
        self.open_calls.lock().unwrap().clone()
    }

    fn dispatched(&self) -> Vec<String> {
        self.chat_calls.lock().unwrap().clone()
    }

    /// The error a "failing" model produces, chosen by name so a single mock can
    /// exercise dispatch failures independently from pre-dispatch skips.
    fn failure_for(model: &str) -> Option<BackendError> {
        const DISPATCH_STATUSES: [u16; 8] = [400, 413, 414, 415, 422, 431, 502, 503];
        if let Some(status) = DISPATCH_STATUSES
            .into_iter()
            .find(|status| model.contains(&format!("dispatch-{status}")))
        {
            let message = if status == 400 {
                "the requested upstream model does not exist"
            } else {
                "upstream rejected the request"
            };
            Some(BackendError::api_status(status, message))
        } else if model.contains("unauthorized") {
            // Describes the *connection* (expired/rotated credential).
            Some(BackendError::api_status(401, "invalid api key"))
        } else if model.contains("notfound") {
            // Describes the *deployment* (model removed from this connection).
            Some(BackendError::api_status(404, "unknown model"))
        } else if model.contains("ratelimit") {
            // Carries forwardable headers, so a test can assert they survive.
            Some(BackendError::api_error(
                "slow down",
                429,
                vec![("retry-after".to_string(), "30".to_string())],
            ))
        } else if model.contains("fail") {
            Some(BackendError::api_status(502, "upstream failed"))
        } else {
            None
        }
    }
}

/// SSE bytes that name the model which served them, so a test can prove the body
/// came from the winner and that no partial bytes leaked from a failed attempt.
fn sse_for(model: &str) -> SseByteStream {
    let usage = if model.contains("omit-usage") {
        ""
    } else {
        r#","usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}"#
    };
    let chunks = vec![
        format!(
            "data: {{\"id\":\"chatcmpl-sf\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"{model}\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\"}},\"finish_reason\":null}}]}}\n\n"
        ),
        format!(
            "data: {{\"id\":\"chatcmpl-sf\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"{model}\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"served-by:{model}\"}},\"finish_reason\":null}}]}}\n\n"
        ),
        format!(
            "data: {{\"id\":\"chatcmpl-sf\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"{model}\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]{usage}}}\n\n"
        ),
        "data: [DONE]\n\n".to_string(),
    ];
    Box::pin(futures::stream::iter(
        chunks
            .into_iter()
            .map(|c| Ok(bytes::Bytes::from(c)))
            .collect::<Vec<_>>(),
    ))
}

fn errored_sse_that_stays_open() -> SseByteStream {
    Box::pin(
        futures::stream::once(async {
            Err(BackendError::api_status(
                502,
                "upstream stream failed after opening",
            ))
        })
        .chain(futures::stream::pending()),
    )
}

fn fixed_chat_response(model: &str) -> ChatCompletionResponse {
    ChatCompletionResponse {
        id: "chatcmpl-sf".to_string(),
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
        usage: (!model.contains("omit-usage")).then_some(Usage {
            prompt_tokens: 1,
            completion_tokens: Some(1),
            total_tokens: 2,
            ..Default::default()
        }),
    }
}

#[async_trait]
impl BackendProvider for MockBackend {
    /// Overridden so the stream-open outcome is under test control — the default
    /// trait implementation would buffer a non-streaming call instead.
    async fn create_chat_completion_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<SseByteStream, BackendError> {
        self.open_calls.lock().unwrap().push(request.model.clone());
        if request.model.contains("stream-error") {
            return Ok(errored_sse_that_stays_open());
        }
        match Self::failure_for(&request.model) {
            Some(e) => Err(e),
            None => Ok(sse_for(&request.model)),
        }
    }

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

    async fn resolve(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        Ok(self.resolved_route(model))
    }

    /// Alternates resolve strictly, and a model named "unresolvable" fails that
    /// resolve — standing in for a typo or a stale entry in the fallback policy.
    async fn resolve_strict(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        if model.contains("unresolvable") {
            return Err(BackendError::ConfigError(format!("no route for {model}")));
        }
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

struct QuotaFixture {
    budget_store: Arc<dyn openai_server::budget::BudgetPolicyStore>,
    rate_store: Arc<dyn RateLimitPolicyStore>,
    backend: Arc<dyn QuotaBackend>,
}

#[derive(Default)]
struct CountingQuota {
    inner: MemoryQuota,
    budget_acquires: AtomicUsize,
    rate_acquires: AtomicUsize,
    mixed_request_acquires: AtomicUsize,
}

#[derive(Default)]
struct FinishRecordingQuota {
    inner: MemoryQuota,
    finishes: AtomicUsize,
    unknown_finishes: AtomicUsize,
}

struct FailingQuota;

#[async_trait]
impl QuotaBackend for FailingQuota {
    fn enabled(&self) -> bool {
        true
    }

    async fn acquire_many(
        &self,
        actions: &[QuotaAction],
        _failed_amount: Option<u64>,
    ) -> Result<QuotaDecision, QuotaBackendError> {
        if actions.is_empty() {
            Ok(QuotaDecision::Allowed(None))
        } else {
            Err(QuotaBackendError::unavailable("test backend failure"))
        }
    }

    async fn finish(
        &self,
        _grant: &QuotaGrant,
        _outcome: QuotaOutcome,
    ) -> Result<(), QuotaBackendError> {
        Ok(())
    }

    async fn renew(&self, _permit_id: uuid::Uuid) -> Result<(), QuotaBackendError> {
        Ok(())
    }

    async fn cleanup(&self, _batch_size: u64) -> Result<(u64, u64), QuotaBackendError> {
        Ok((0, 0))
    }
}

#[async_trait]
impl QuotaBackend for CountingQuota {
    fn enabled(&self) -> bool {
        true
    }

    async fn acquire_many(
        &self,
        actions: &[QuotaAction],
        failed_amount: Option<u64>,
    ) -> Result<QuotaDecision, QuotaBackendError> {
        match actions.first().map(|action| action.source) {
            Some(QuotaSource::BudgetLimit) => {
                self.budget_acquires.fetch_add(1, Ordering::SeqCst);
            }
            Some(QuotaSource::RateLimit) => {
                self.rate_acquires.fetch_add(1, Ordering::SeqCst);
            }
            None => {}
        }
        if actions
            .iter()
            .any(|action| action.source == QuotaSource::BudgetLimit)
            && actions
                .iter()
                .any(|action| action.policy_id == "counted-global-rate")
        {
            self.mixed_request_acquires.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.acquire_many(actions, failed_amount).await
    }

    async fn finish(
        &self,
        grant: &QuotaGrant,
        outcome: QuotaOutcome,
    ) -> Result<(), QuotaBackendError> {
        self.inner.finish(grant, outcome).await
    }

    async fn renew(&self, permit_id: uuid::Uuid) -> Result<(), QuotaBackendError> {
        self.inner.renew(permit_id).await
    }

    async fn cleanup(&self, batch_size: u64) -> Result<(u64, u64), QuotaBackendError> {
        self.inner.cleanup(batch_size).await
    }
}

#[async_trait]
impl QuotaBackend for FinishRecordingQuota {
    fn enabled(&self) -> bool {
        true
    }

    async fn acquire_many(
        &self,
        actions: &[QuotaAction],
        failed_amount: Option<u64>,
    ) -> Result<QuotaDecision, QuotaBackendError> {
        self.inner.acquire_many(actions, failed_amount).await
    }

    async fn finish(
        &self,
        grant: &QuotaGrant,
        outcome: QuotaOutcome,
    ) -> Result<(), QuotaBackendError> {
        self.finishes.fetch_add(1, Ordering::SeqCst);
        if matches!(outcome, QuotaOutcome::Unknown) {
            self.unknown_finishes.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.finish(grant, outcome).await
    }

    async fn renew(&self, permit_id: uuid::Uuid) -> Result<(), QuotaBackendError> {
        self.inner.renew(permit_id).await
    }

    async fn cleanup(&self, batch_size: u64) -> Result<(u64, u64), QuotaBackendError> {
        self.inner.cleanup(batch_size).await
    }
}

async fn state_with(
    backend: Arc<MockBackend>,
    api_key_store: Arc<dyn ApiKeyStore>,
    fallbacks: FallbackPolicy,
    quota: QuotaFixture,
) -> AppState {
    let budget_store = quota.budget_store;
    let quota_resolver = Arc::new(openai_server::quota_policy::QuotaPolicyResolver::new(
        "develop".to_string(),
        budget_store.clone(),
        quota.rate_store.clone(),
    ));
    // No test in this file presents a Chalk JWT (auth is always `Bearer {key}`
    // against the seeded api-key store), so the builder's default empty
    // federated-auth registry is equivalent to the previous explicit
    // `OidcRegistry::from_config(&[])` and needs no override.
    AppStateBuilder::new(backend)
        .api_key_store(api_key_store)
        .fallback_policy(fallbacks)
        .budget_store(budget_store)
        .rate_limit_store(quota.rate_store)
        .quota_resolver(quota_resolver)
        .quota_backend(quota.backend)
        .build()
        .await
}

async fn key_store(permission: ApiKeyPermission) -> Arc<dyn ApiKeyStore> {
    let hasher = KeyHasher::new(b"test-pepper".to_vec());
    let store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(hasher.clone()));
    store
        .insert_raw(openai_server::api_keys::ApiKey {
            key_hash: hasher.hash(TEST_KEY),
            key_prefix: openai_server::hashing::key_prefix(TEST_KEY),
            description: "streaming fallback test key".to_string(),
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
            "/v1/chat/completions",
            post(openai_server::handlers::chat::create_chat_completion),
        )
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, auth_middleware))
}

async fn boot_with(permission: ApiKeyPermission, fallbacks: FallbackPolicy) -> Setup {
    boot_with_rate_limit(
        permission,
        fallbacks,
        QuotaFixture {
            budget_store: openai_server::budget::from_core_store(None, "develop".to_string()),
            rate_store: openai_server::ratelimit::from_core_store(None, "develop".to_string()),
            backend: Arc::new(MemoryQuota::new()),
        },
    )
    .await
}

async fn boot_with_rate_limit(
    permission: ApiKeyPermission,
    fallbacks: FallbackPolicy,
    quota: QuotaFixture,
) -> Setup {
    let backend = Arc::new(MockBackend::default());
    let store = key_store(permission).await;
    let state = state_with(backend.clone(), store, fallbacks, quota).await;
    let app = app(state);
    let base = crate::support::mock::serve(app).await;
    let port = base.rsplit(':').next().unwrap().parse().unwrap();
    Setup { backend, port }
}

async fn exhausted_rate_backend(connection_id: Option<&str>, model: Option<&str>) -> QuotaFixture {
    let rate_store = Arc::new(MemoryRateLimitPolicyStore::new("develop".to_string()));
    let backend = Arc::new(MemoryQuota::new());
    let key_hash = KeyHasher::new(b"test-pepper".to_vec()).hash(TEST_KEY);
    let selector = connection_id.unwrap_or("global");
    let quota_policy_id = format!("quota-exhausted-{selector}");
    rate_store
        .create(RateLimitPolicy {
            quota_policy_id: quota_policy_id.clone(),
            id: format!("exhausted-{selector}"),
            limit_type: "requests_per_minute".to_string(),
            limit_value: 1,
            burst_value: Some(1),
            connection_id: connection_id.map(str::to_string),
            models: model.into_iter().map(str::to_string).collect(),
            scope: "api_key".to_string(),
            scope_id: key_hash.clone(),
            enabled: true,
            created_at: chrono::Utc::now(),
        })
        .await
        .expect("create exhausted test policy");
    assert!(matches!(
        backend
            .acquire_many(
                &[QuotaAction {
                    policy_id: quota_policy_id,
                    environment_id: "develop".to_string(),
                    source: QuotaSource::RateLimit,
                    algorithm: QuotaAlgorithm::TokenBucket,
                    meter: QuotaMeter::Requests,
                    limit_value: 1,
                    burst: Some(1),
                    amount: 1,
                    window_start: None,
                    window_end: None,
                }],
                None
            )
            .await
            .expect("consume bucket"),
        QuotaDecision::Allowed(None)
    ));
    let _ = key_hash;
    QuotaFixture {
        budget_store: openai_server::budget::from_core_store(None, "develop".to_string()),
        rate_store,
        backend,
    }
}

async fn token_rate_fixture(
    connection_id: Option<&str>,
    model: Option<&str>,
) -> (QuotaFixture, Arc<CountingQuota>) {
    let rate_store = Arc::new(MemoryRateLimitPolicyStore::new("develop".to_string()));
    let key_hash = KeyHasher::new(b"test-pepper".to_vec()).hash(TEST_KEY);
    rate_store
        .create(RateLimitPolicy {
            quota_policy_id: format!("unknown-token-rate-{}", connection_id.unwrap_or("global")),
            id: format!(
                "unknown-token-rate-source-{}",
                connection_id.unwrap_or("global")
            ),
            limit_type: "tokens_per_minute".to_string(),
            limit_value: 60_000,
            burst_value: Some(60_000),
            connection_id: connection_id.map(str::to_string),
            models: model.into_iter().map(str::to_string).collect(),
            scope: "api_key".to_string(),
            scope_id: key_hash,
            enabled: true,
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    let backend = Arc::new(CountingQuota::default());
    (
        QuotaFixture {
            budget_store: openai_server::budget::from_core_store(None, "develop".to_string()),
            rate_store,
            backend: backend.clone(),
        },
        backend,
    )
}

async fn recording_concurrency_fixture() -> (QuotaFixture, Arc<FinishRecordingQuota>) {
    let rate_store = Arc::new(MemoryRateLimitPolicyStore::new("develop".to_string()));
    let key_hash = KeyHasher::new(b"test-pepper".to_vec()).hash(TEST_KEY);
    rate_store
        .create(RateLimitPolicy {
            quota_policy_id: "stream-error-concurrency".to_string(),
            id: "stream-error-concurrency-source".to_string(),
            limit_type: "concurrent_requests".to_string(),
            limit_value: 1,
            burst_value: None,
            connection_id: None,
            models: Vec::new(),
            scope: "api_key".to_string(),
            scope_id: key_hash,
            enabled: true,
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    let backend = Arc::new(FinishRecordingQuota::default());
    (
        QuotaFixture {
            budget_store: openai_server::budget::from_core_store(None, "develop".to_string()),
            rate_store,
            backend: backend.clone(),
        },
        backend,
    )
}

async fn counted_budget_and_rate_fixture(
    primary: &str,
    alternate: &str,
) -> (QuotaFixture, Arc<CountingQuota>) {
    let key_hash = KeyHasher::new(b"test-pepper".to_vec()).hash(TEST_KEY);
    let budget_store = Arc::new(openai_server::budget::MemoryBudgetPolicyStore::new(
        "develop".to_string(),
    ));
    budget_store
        .insert(openai_server::budget::BudgetPolicy {
            quota_policy_id: "counted-budget".to_string(),
            id: "counted-budget-source".to_string(),
            environment_id: "develop".to_string(),
            scope: "api_key".to_string(),
            scope_id: key_hash.clone(),
            period: "daily".to_string(),
            window_type: "calendar".to_string(),
            window_size: sqlx::postgres::types::PgInterval {
                months: 0,
                days: 1,
                microseconds: 0,
            },
            anchor: None,
            timezone: "UTC".to_string(),
            limit_kind: "tokens".to_string(),
            limit_amount: 1_000_000,
            enabled: true,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
    let rate_store = Arc::new(MemoryRateLimitPolicyStore::new("develop".to_string()));
    for (connection_id, model) in [("openai", primary), ("anthropic", alternate)] {
        rate_store
            .create(RateLimitPolicy {
                quota_policy_id: format!("counted-rate-{connection_id}"),
                id: format!("counted-rate-source-{connection_id}"),
                limit_type: "requests_per_minute".to_string(),
                limit_value: 60_000,
                burst_value: Some(1_000),
                connection_id: Some(connection_id.to_string()),
                models: vec![model.to_string()],
                scope: "api_key".to_string(),
                scope_id: key_hash.clone(),
                enabled: true,
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
    }
    rate_store
        .create(RateLimitPolicy {
            quota_policy_id: "counted-global-rate".to_string(),
            id: "counted-global-rate-source".to_string(),
            limit_type: "requests_per_minute".to_string(),
            limit_value: 60_000,
            burst_value: Some(1_000),
            connection_id: None,
            models: Vec::new(),
            scope: "api_key".to_string(),
            scope_id: key_hash.clone(),
            enabled: true,
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    let backend = Arc::new(CountingQuota::default());
    (
        QuotaFixture {
            budget_store,
            rate_store,
            backend: backend.clone(),
        },
        backend,
    )
}

fn budget_fixture(limit_amount: u64) -> QuotaFixture {
    let key_hash = KeyHasher::new(b"test-pepper".to_vec()).hash(TEST_KEY);
    let budget_store = Arc::new(openai_server::budget::MemoryBudgetPolicyStore::new(
        "develop".to_string(),
    ));
    budget_store
        .insert(openai_server::budget::BudgetPolicy {
            quota_policy_id: "fallback-budget".to_string(),
            id: "fallback-budget-source".to_string(),
            environment_id: "develop".to_string(),
            scope: "api_key".to_string(),
            scope_id: key_hash,
            period: "daily".to_string(),
            window_type: "calendar".to_string(),
            window_size: sqlx::postgres::types::PgInterval {
                months: 0,
                days: 1,
                microseconds: 0,
            },
            anchor: None,
            timezone: "UTC".to_string(),
            limit_kind: "tokens".to_string(),
            limit_amount,
            enabled: true,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
    QuotaFixture {
        budget_store,
        rate_store: Arc::new(MemoryRateLimitPolicyStore::new("develop".to_string())),
        backend: Arc::new(MemoryQuota::new()),
    }
}

async fn boot(fallbacks: FallbackPolicy) -> Setup {
    boot_with(ApiKeyPermission::default(), fallbacks).await
}

fn policy(primary: &str, alternates: &[&str]) -> FallbackPolicy {
    let mut map = HashMap::new();
    map.insert(
        primary.to_string(),
        alternates.iter().map(|s| s.to_string()).collect(),
    );
    FallbackPolicy::from_map(map)
}

async fn post_stream(port: u16, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(TEST_KEY)
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
        }))
        .send()
        .await
        .expect("send")
}

async fn post_stream_with_max_tokens(port: u16, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(TEST_KEY)
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 10,
            "stream": true,
        }))
        .send()
        .await
        .expect("send")
}

async fn post_stream_with_tools(port: u16, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(TEST_KEY)
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "look it up"}],
            "tools": [{
                "type": "function",
                "function": {"name": "lookup", "parameters": {"type": "object"}}
            }],
            "stream": true,
        }))
        .send()
        .await
        .expect("send")
}

async fn post_non_stream(port: u16, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(TEST_KEY)
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
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
async fn stream_open_failure_falls_back_before_first_byte() {
    // The headline case: the primary's stream fails to open, an alternate opens,
    // and the client sees a clean 200 it cannot distinguish from a first-try win.
    let setup = boot(policy("openai/fail-primary", &["openai/good-alt"])).await;

    let resp = post_stream(setup.port, "openai/fail-primary").await;

    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "content-type").as_deref(),
        Some("text/event-stream")
    );
    assert_eq!(
        header(&resp, "x-fallback-model").as_deref(),
        Some("openai/good-alt")
    );
    // The routed-model header names what ACTUALLY served, not what was requested.
    assert_eq!(
        header(&resp, "x-chalk-routed-model").as_deref(),
        Some("openai/good-alt")
    );

    let body = resp.text().await.unwrap();
    assert!(
        body.contains("served-by:openai/good-alt"),
        "body must come from the winning alternate; got: {body}"
    );
    // Retry-before-first-byte: nothing from the failed attempt reached the client.
    assert!(
        !body.contains("openai/fail-primary"),
        "no bytes may leak from the failed attempt; got: {body}"
    );
    assert_eq!(
        setup.backend.opened(),
        vec!["openai/fail-primary", "openai/good-alt"],
        "primary attempted first, then the alternate"
    );
}

#[tokio::test]
async fn a_non_clean_stream_settles_unknown_quota_before_the_body_is_dropped() {
    let model = "openai/stream-error-unbounded";
    let (quota, finishes) = recording_concurrency_fixture().await;
    let setup =
        boot_with_rate_limit(ApiKeyPermission::default(), FallbackPolicy::empty(), quota).await;

    let response = post_stream(setup.port, model).await;
    assert_eq!(response.status(), 200);
    let mut body = response.bytes_stream();
    let first = tokio::time::timeout(std::time::Duration::from_secs(1), body.next())
        .await
        .expect("stream error response should arrive")
        .expect("stream should yield an error event")
        .expect("router should translate the upstream error into SSE bytes");
    assert!(String::from_utf8_lossy(&first).contains("backend_error"));

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while finishes.unknown_finishes.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("non-clean quota settlement should not wait for body drop or lease expiry");
    assert_eq!(finishes.finishes.load(Ordering::SeqCst), 1);

    // Keep the response body alive through the assertion: this proves the
    // explicit non-clean terminal path settled the permit, not its Drop impl.
    drop(body);
}

#[tokio::test]
async fn fallback_charges_request_quotas_once_and_local_rates_each_dispatch() {
    let primary = "openai/fail-primary-counted";
    let alternate = "anthropic/good-alt-counted";
    let (quota, counts) = counted_budget_and_rate_fixture(primary, alternate).await;
    let setup = boot_with_rate_limit(
        ApiKeyPermission::default(),
        policy(primary, &[alternate]),
        quota,
    )
    .await;

    let response = post_stream_with_max_tokens(setup.port, primary).await;

    assert_eq!(response.status(), 200);
    assert_eq!(counts.budget_acquires.load(Ordering::SeqCst), 1);
    assert_eq!(counts.rate_acquires.load(Ordering::SeqCst), 2);
    assert_eq!(counts.mixed_request_acquires.load(Ordering::SeqCst), 1);
    assert_eq!(setup.backend.opened(), [primary, alternate]);
}

#[tokio::test]
async fn missing_usage_after_fallback_keeps_original_unicode_tool_input_estimate() {
    let primary = "openai/fail-long-primary-with-original-prompt";
    let alternate = "anthropic/omit-usage";
    for streaming in [false, true] {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
        let backend = Arc::new(MockBackend::default());
        let (quota, counts) = counted_budget_and_rate_fixture(primary, alternate).await;
        let state = state_with(
            backend.clone(),
            key_store(ApiKeyPermission::default()).await,
            policy(primary, &[alternate]),
            quota,
        )
        .await;
        let value = serde_json::json!({
            "model": primary, "stream": streaming, "max_tokens": 512,
            "messages": [
                {"role":"assistant", "content":null, "tool_calls":[{"id":"call-1", "type":"function",
                    "function":{"name":"lookup", "arguments":"{\"city\":\"東京\"}"}}]},
                {"role":"tool", "tool_call_id":"call-1", "content":"晴れ🦀\n"}
            ],
            "tools": [{"type":"function", "function":{"name":"lookup", "parameters":{"type":"object"}}}]
        });
        let bytes = serde_json::to_vec(&value).unwrap();
        let expected_prompt = bytes.len() as u64 + 32;
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let request = axum::http::Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {TEST_KEY}"))
                .body(axum::body::Body::from(bytes))
                .unwrap();
            let response = app(state).oneshot(request).await.unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(response.headers()["x-fallback-model"], alternate);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            if streaming {
                assert!(String::from_utf8_lossy(&body).contains("[DONE]"));
            }
        }
        for flush in provider.force_flush() {
            flush.unwrap();
        }
        let spans = exporter.get_finished_spans().unwrap();
        let completed: Vec<_> = spans
            .iter()
            .filter(|span| span.name == "chat.completion")
            .collect();
        assert_eq!(
            completed.len(),
            1,
            "one usage-bearing completion per logical request"
        );
        let prompt = completed[0]
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == "llm.token_count.prompt")
            .unwrap();
        assert_eq!(prompt.value.to_string(), expected_prompt.to_string());
        assert_eq!(counts.budget_acquires.load(Ordering::SeqCst), 1);
        assert_eq!(counts.rate_acquires.load(Ordering::SeqCst), 2);
        if streaming {
            assert_eq!(backend.opened(), [primary, alternate]);
        } else {
            assert_eq!(backend.dispatched(), [primary, alternate]);
        }
    }
}

#[tokio::test]
async fn fallback_reservation_uses_the_largest_viable_route_ceiling() {
    let primary = "openai/fail-cap-64-primary";
    let alternate = "anthropic/good-cap-1024-alternate";
    let setup = boot_with_rate_limit(
        ApiKeyPermission::default(),
        policy(primary, &[alternate]),
        budget_fixture(500),
    )
    .await;

    let response = post_stream(setup.port, primary).await;

    assert_eq!(response.status(), 429);
    assert!(setup.backend.opened().is_empty());
}

#[tokio::test]
async fn an_unbounded_fallback_makes_the_logical_budget_reservation_unknown() {
    let primary = "openai/good-cap-64-primary";
    let alternate = "anthropic/good-unbounded-alternate";
    let setup = boot_with_rate_limit(
        ApiKeyPermission::default(),
        policy(primary, &[alternate]),
        budget_fixture(500),
    )
    .await;

    let response = post_stream(setup.port, primary).await;

    assert_eq!(response.status(), 400);
    assert!(setup.backend.opened().is_empty());
}

#[tokio::test]
async fn unknown_global_tpm_is_rejected_before_a_zero_sized_acquisition() {
    let primary = "openai/good-unbounded-global-tpm";
    let (quota, acquires) = token_rate_fixture(None, None).await;
    let setup =
        boot_with_rate_limit(ApiKeyPermission::default(), FallbackPolicy::empty(), quota).await;

    let response = post_stream(setup.port, primary).await;

    assert_eq!(response.status(), 400);
    assert!(setup.backend.opened().is_empty());
    assert_eq!(acquires.rate_acquires.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unknown_route_tpm_is_rejected_before_dispatch() {
    let primary = "openai/good-unbounded-route-tpm";
    let (quota, acquires) = token_rate_fixture(Some("openai"), Some(primary)).await;
    let setup =
        boot_with_rate_limit(ApiKeyPermission::default(), FallbackPolicy::empty(), quota).await;

    let response = post_stream(setup.port, primary).await;

    assert_eq!(response.status(), 400);
    assert!(setup.backend.opened().is_empty());
    assert_eq!(acquires.rate_acquires.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn explicit_output_cap_is_route_independent_during_fallback() {
    let primary = "openai/fail-cap-64-primary";
    let alternate = "anthropic/good-cap-1024-alternate";
    let setup = boot_with_rate_limit(
        ApiKeyPermission::default(),
        policy(primary, &[alternate]),
        budget_fixture(500),
    )
    .await;

    let response = post_stream_with_max_tokens(setup.port, primary).await;

    assert_eq!(response.status(), 200);
    assert_eq!(setup.backend.opened(), [primary, alternate]);
}

#[tokio::test]
async fn image_parts_admit_as_a_soft_cap_over_the_visible_input() {
    let model = "openai/good-cap-64-image";
    let setup = boot_with_rate_limit(
        ApiKeyPermission::default(),
        FallbackPolicy::empty(),
        budget_fixture(500),
    )
    .await;
    // Counted as text, the 4 KiB image alone would exceed the 500-token budget;
    // its token cost is the provider's, so admission counts only the text and
    // holds the rest to a soft cap settled from reported usage.
    let image = format!("data:image/png;base64,{}", "A".repeat(4_096));
    let response = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{}/v1/chat/completions",
            setup.port
        ))
        .bearer_auth(TEST_KEY)
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "describe"},
                {"type": "image_url", "image_url": {"url": image}},
            ]}],
            "max_tokens": 10,
            "stream": true,
        }))
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), 200);
    assert_eq!(setup.backend.opened(), [model]);
}

#[tokio::test]
async fn quota_backend_failure_fails_open_instead_of_returning_429() {
    let model = "openai/good-quota-backend-error";
    let key_hash = KeyHasher::new(b"test-pepper".to_vec()).hash(TEST_KEY);
    let rate_store = Arc::new(MemoryRateLimitPolicyStore::new("develop".to_string()));
    rate_store
        .create(RateLimitPolicy {
            quota_policy_id: "fail-open-rate".to_string(),
            id: "fail-open-rate-source".to_string(),
            limit_type: "requests_per_minute".to_string(),
            limit_value: 1,
            burst_value: Some(1),
            connection_id: Some("openai".to_string()),
            models: vec![model.to_string()],
            scope: "api_key".to_string(),
            scope_id: key_hash,
            enabled: true,
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    let setup = boot_with_rate_limit(
        ApiKeyPermission::default(),
        FallbackPolicy::empty(),
        QuotaFixture {
            budget_store: openai_server::budget::from_core_store(None, "develop".to_string()),
            rate_store,
            backend: Arc::new(FailingQuota),
        },
    )
    .await;

    let response = post_non_stream(setup.port, model).await;

    assert_eq!(response.status(), 200);
    assert_eq!(setup.backend.dispatched(), [model]);
}

#[tokio::test]
async fn capability_incompatible_fallback_is_skipped_before_dispatch() {
    let setup = boot(policy(
        "openai/fail-primary",
        &["custom/no-tools-alt", "openai/good-alt"],
    ))
    .await;

    let resp = post_stream_with_tools(setup.port, "openai/fail-primary").await;

    assert_eq!(resp.status(), 200);
    assert_eq!(
        setup.backend.opened(),
        vec!["openai/fail-primary", "openai/good-alt"],
        "the explicitly incompatible candidate must never be dispatched"
    );
}

#[tokio::test]
async fn successful_primary_stream_is_untouched() {
    // The happy path must be byte-identical to pre-RFP behavior: one open, no
    // routing headers, and no alternate attempted.
    let setup = boot(policy("openai/good-primary", &["openai/good-alt"])).await;

    let resp = post_stream(setup.port, "openai/good-primary").await;

    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-fallback-model"), None);
    assert_eq!(header(&resp, "x-chalk-routed-model"), None);
    let body = resp.text().await.unwrap();
    assert!(body.contains("served-by:openai/good-primary"));
    assert_eq!(setup.backend.opened(), vec!["openai/good-primary"]);
}

#[tokio::test]
async fn every_candidate_failing_returns_a_real_non_2xx() {
    // No 200 was ever committed, so the failure is a real status the client can
    // catch on `!response.ok` — not an in-band SSE error event.
    let setup = boot(policy(
        "openai/fail-primary",
        &["openai/dispatch-503-alt-1", "openai/dispatch-502-alt-2"],
    ))
    .await;

    let resp = post_stream(setup.port, "openai/fail-primary").await;

    assert_eq!(resp.status(), 502, "last upstream status is preserved");
    assert_ne!(
        header(&resp, "content-type").as_deref(),
        Some("text/event-stream"),
        "a pre-open failure must not masquerade as a stream"
    );
    assert_eq!(
        setup.backend.opened(),
        vec![
            "openai/fail-primary",
            "openai/dispatch-503-alt-1",
            "openai/dispatch-502-alt-2"
        ],
        "every candidate is attempted before giving up"
    );
}

#[tokio::test]
async fn failed_stream_still_stamps_the_route_span_attributes() {
    // Regression guard for RFP-0002 §5.6: attribution is stamped up front from
    // the primary and only *re-stamped* from the winner. If it were merely moved
    // to post-win, the span for a total failure — the one most worth having —
    // would carry no system/model at all.
    let exporter = InMemorySpanExporter::default();
    let provider = TracerProvider::builder()
        .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));

    let backend = Arc::new(MockBackend::default());
    let store = key_store(ApiKeyPermission::default()).await;
    let state = state_with(
        backend.clone(),
        store,
        policy("anthropic/fail-primary", &["anthropic/fail-alt"]),
        QuotaFixture {
            budget_store: openai_server::budget::from_core_store(None, "develop".to_string()),
            rate_store: openai_server::ratelimit::from_core_store(None, "develop".to_string()),
            backend: openai_server::quota::from_kind(
                openai_server::config::QuotaBackendKind::Disabled,
                None,
            ),
        },
    )
    .await;

    // Driven with `oneshot` rather than a spawned server: `set_default` installs
    // the subscriber for *this* thread only, so a request handled on a spawned
    // task would export nothing.
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let req = axum::http::Request::builder()
            .method(axum::http::Method::POST)
            .uri("/v1/chat/completions")
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .header(
                axum::http::header::AUTHORIZATION,
                format!("Bearer {TEST_KEY}"),
            )
            .body(axum::body::Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "model": "anthropic/fail-primary",
                    "messages": [{"role": "user", "content": "hi"}],
                    "stream": true,
                }))
                .unwrap(),
            ))
            .unwrap();
        let resp = app(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 502);
    }

    for r in provider.force_flush() {
        r.expect("flush");
    }
    let spans = exporter.get_finished_spans().unwrap();
    let completion: Vec<&SpanData> = spans
        .iter()
        .filter(|s| s.name.as_ref() == "chat.completion")
        .collect();
    assert!(!completion.is_empty(), "expected a chat.completion span");

    let attrs: HashMap<String, String> = completion[0]
        .attributes
        .iter()
        .map(|kv| (kv.key.to_string(), kv.value.to_string()))
        .collect();
    assert_eq!(
        attrs.get("llm.system").map(String::as_str),
        Some("anthropic"),
        "a fully-failed stream must still be attributed to the primary it tried"
    );
    assert_eq!(
        attrs.get("llm.model_name").map(String::as_str),
        Some("anthropic/fail-primary")
    );
    assert_eq!(attrs.get("llm.stream").map(String::as_str), Some("true"));
}

#[tokio::test]
async fn upstream_request_statuses_are_terminal_before_stream_open() {
    for status in [400, 413, 414, 415, 422, 431] {
        let primary = format!("openai/dispatch-{status}-primary");
        let setup = boot(policy(&primary, &["openai/good-alt"])).await;
        let resp = post_stream(setup.port, &primary).await;
        assert_eq!(resp.status(), status);
        assert_eq!(header(&resp, "x-fallback-model"), None);
        assert_eq!(setup.backend.opened(), vec![primary]);
    }
}

#[tokio::test]
async fn upstream_request_statuses_are_terminal_non_streaming() {
    for status in [400, 413, 414, 415, 422, 431] {
        let primary = format!("openai/dispatch-{status}-primary");
        let setup = boot(policy(&primary, &["openai/good-alt"])).await;
        let resp = post_non_stream(setup.port, &primary).await;
        assert_eq!(resp.status(), status);
        assert_eq!(header(&resp, "x-fallback-model"), None);
        assert_eq!(setup.backend.dispatched(), vec![primary]);
    }
}

#[tokio::test]
async fn upstream_5xx_still_walks_the_alternates_non_streaming() {
    // Guard the ordinary provider-failure path alongside the request-status
    // coverage above.
    let setup = boot(policy("openai/fail-primary", &["openai/good-alt"])).await;

    let resp = post_non_stream(setup.port, "openai/fail-primary").await;

    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "x-fallback-model").as_deref(),
        Some("openai/good-alt")
    );
    assert_eq!(
        setup.backend.dispatched(),
        vec!["openai/fail-primary", "openai/good-alt"]
    );
}

#[tokio::test]
async fn permission_denied_alternate_is_skipped_not_terminal() {
    // The subtle one (RFP-0002 §5.3): a denied alternate is a *skip* reason, not
    // a dispatch outcome. It must never replace the last real upstream error or
    // prevent the permitted alternate behind it from serving.
    let permission = ApiKeyPermission {
        provider: Some("anthropic".to_string()),
        ..Default::default()
    };
    let setup = boot_with(
        permission,
        policy(
            "anthropic/fail-primary",
            &["openai/good-but-denied", "anthropic/good-alt"],
        ),
    )
    .await;

    let resp = post_stream(setup.port, "anthropic/fail-primary").await;

    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "x-fallback-model").as_deref(),
        Some("anthropic/good-alt"),
        "the chain must continue past a denied alternate"
    );
    let opened = setup.backend.opened();
    assert!(
        !opened.contains(&"openai/good-but-denied".to_string()),
        "a denied alternate must never be dispatched; got: {opened:?}"
    );
    assert_eq!(opened, vec!["anthropic/fail-primary", "anthropic/good-alt"],);
}

#[tokio::test]
async fn primary_401_is_terminal() {
    let setup = boot(policy("openai/unauthorized-primary", &["openai/good-alt"])).await;
    let resp = post_stream(setup.port, "openai/unauthorized-primary").await;
    assert_eq!(resp.status(), 401);
    assert_eq!(header(&resp, "x-fallback-model"), None);
    assert_eq!(setup.backend.opened(), vec!["openai/unauthorized-primary"]);
}

#[tokio::test]
async fn primary_404_is_terminal() {
    let setup = boot(policy("openai/notfound-primary", &["openai/good-alt"])).await;
    let resp = post_stream(setup.port, "openai/notfound-primary").await;
    assert_eq!(resp.status(), 404);
    assert_eq!(header(&resp, "x-fallback-model"), None);
    assert_eq!(setup.backend.opened(), vec!["openai/notfound-primary"]);
}

#[tokio::test]
async fn primary_401_is_terminal_non_streaming() {
    let setup = boot(policy("openai/unauthorized-primary", &["openai/good-alt"])).await;
    let resp = post_non_stream(setup.port, "openai/unauthorized-primary").await;
    assert_eq!(resp.status(), 401);
    assert_eq!(header(&resp, "x-fallback-model"), None);
    assert_eq!(
        setup.backend.dispatched(),
        vec!["openai/unauthorized-primary"]
    );
}

#[tokio::test]
async fn denied_final_alternate_does_not_replace_the_primary_dispatch_error() {
    // The key is fenced to anthropic. The primary is permitted and fails with a
    // real 502; the only alternate is a provider the key can't reach, so it is
    // *skipped* — never dispatched. The client must see the primary's 502, not a
    // spurious 403 synthesized from a candidate that was never attempted.
    let permission = ApiKeyPermission {
        provider: Some("anthropic".to_string()),
        ..Default::default()
    };
    let setup = boot_with(
        permission,
        policy("anthropic/fail-primary", &["openai/denied-alt"]),
    )
    .await;

    let resp = post_stream(setup.port, "anthropic/fail-primary").await;

    assert_eq!(
        resp.status(),
        502,
        "the answer must be the primary's real dispatch failure, not a skip reason"
    );
    assert_eq!(
        setup.backend.opened(),
        vec!["anthropic/fail-primary"],
        "the denied alternate must never be dispatched"
    );
}

#[tokio::test]
async fn denied_final_alternate_preserves_the_primary_rate_limit_headers() {
    // The same shape, with a 429 primary: dropping its status for a synthesized
    // 403 would also drop `Retry-After`, which is the header clients back off on.
    let permission = ApiKeyPermission {
        provider: Some("anthropic".to_string()),
        ..Default::default()
    };
    let setup = boot_with(
        permission,
        policy("anthropic/ratelimit-primary", &["openai/denied-alt"]),
    )
    .await;

    let resp = post_stream(setup.port, "anthropic/ratelimit-primary").await;

    assert_eq!(resp.status(), 429);
    assert_eq!(
        header(&resp, "retry-after").as_deref(),
        Some("30"),
        "a skipped alternate must not cost the caller the primary's Retry-After"
    );
}

#[tokio::test]
async fn router_limited_primary_is_skipped_before_dispatch_and_falls_back() {
    let primary = "openai/limited-primary";
    let alternate = "anthropic/good-alt";
    let limiter = exhausted_rate_backend(Some("openai"), Some(primary)).await;
    let setup = boot_with_rate_limit(
        ApiKeyPermission::default(),
        policy(primary, &[alternate]),
        limiter,
    )
    .await;

    let response = post_stream(setup.port, primary).await;

    assert_eq!(response.status(), 200);
    assert_eq!(
        header(&response, "x-fallback-model").as_deref(),
        Some(alternate)
    );
    assert_eq!(
        setup.backend.opened(),
        vec![alternate],
        "the exhausted primary must not contact its upstream"
    );
}

#[tokio::test]
async fn limited_fallback_does_not_replace_a_real_primary_failure() {
    let primary = "openai/fail-primary";
    let alternate = "anthropic/limited-alt";
    let limiter = exhausted_rate_backend(Some("anthropic"), Some(alternate)).await;
    let setup = boot_with_rate_limit(
        ApiKeyPermission::default(),
        policy(primary, &[alternate]),
        limiter,
    )
    .await;

    let response = post_stream(setup.port, primary).await;

    assert_eq!(response.status(), 502);
    assert_eq!(
        setup.backend.opened(),
        vec![primary],
        "a rate-limited fallback is a pre-dispatch skip"
    );
}

#[tokio::test]
async fn router_rate_limit_returns_retry_metadata_without_dispatch() {
    let primary = "openai/limited-primary";
    let limiter = exhausted_rate_backend(Some("openai"), Some(primary)).await;
    let setup = boot_with_rate_limit(
        ApiKeyPermission::default(),
        FallbackPolicy::empty(),
        limiter,
    )
    .await;

    let response = post_non_stream(setup.port, primary).await;

    assert_eq!(response.status(), 429);
    assert_eq!(
        header(&response, "x-chalk-ratelimit-type").as_deref(),
        Some("requests_per_minute")
    );
    assert!(header(&response, "retry-after").is_some());
    assert!(setup.backend.dispatched().is_empty());
}

#[tokio::test]
async fn global_rate_limit_rejects_once_before_any_dispatch_or_fallback() {
    let primary = "openai/global-limited-primary";
    let alternate = "anthropic/global-limited-alternate";
    let limiter = exhausted_rate_backend(None, None).await;
    let setup = boot_with_rate_limit(
        ApiKeyPermission::default(),
        policy(primary, &[alternate]),
        limiter,
    )
    .await;

    let response = post_non_stream(setup.port, primary).await;

    assert_eq!(response.status(), 429);
    assert_eq!(
        header(&response, "x-chalk-ratelimit-type").as_deref(),
        Some("requests_per_minute")
    );
    assert!(setup.backend.dispatched().is_empty());
}

#[tokio::test]
async fn unresolvable_alternate_does_not_replace_the_primary_dispatch_error() {
    // A typo or stale entry in the fallback policy fails strict resolution. That
    // candidate was never dispatched, so it must not turn a real upstream 502
    // into an internal config error.
    let setup = boot(policy("openai/fail-primary", &["openai/unresolvable-alt"])).await;

    let resp = post_stream(setup.port, "openai/fail-primary").await;

    assert_eq!(
        resp.status(),
        502,
        "the client sees the provider error, not the resolver's"
    );
    assert_eq!(setup.backend.opened(), vec!["openai/fail-primary"]);
}

#[tokio::test]
async fn a_skipped_alternate_still_lets_a_later_one_win() {
    // Guard the interaction between the two fixes: skipping without recording an
    // error must not also stop the loop. An unresolvable alternate sits between
    // the failing primary and a good alternate.
    let setup = boot(policy(
        "openai/fail-primary",
        &["openai/unresolvable-alt", "openai/good-alt"],
    ))
    .await;

    let resp = post_stream(setup.port, "openai/fail-primary").await;

    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "x-fallback-model").as_deref(),
        Some("openai/good-alt")
    );
    assert_eq!(
        setup.backend.opened(),
        vec!["openai/fail-primary", "openai/good-alt"]
    );
}

#[tokio::test]
async fn streaming_without_a_fallback_list_reports_the_primary_failure() {
    // Unchanged behavior when the policy is empty: a pre-open failure is still a
    // real non-2xx, and exactly one open is attempted.
    let setup = boot(FallbackPolicy::empty()).await;

    let resp = post_stream(setup.port, "openai/fail-primary").await;

    assert_eq!(resp.status(), 502);
    assert_eq!(setup.backend.opened(), vec!["openai/fail-primary"]);
}

/// Every admission fails with the configured backend error.
struct FailingAdmissionQuota {
    error: fn() -> QuotaBackendError,
    separate_calls: AtomicUsize,
    combined_calls: AtomicUsize,
}

impl FailingAdmissionQuota {
    fn new(error: fn() -> QuotaBackendError) -> Self {
        Self {
            error,
            separate_calls: AtomicUsize::new(0),
            combined_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl QuotaBackend for FailingAdmissionQuota {
    fn enabled(&self) -> bool {
        true
    }

    async fn acquire_many(
        &self,
        _actions: &[QuotaAction],
        _failed: Option<u64>,
    ) -> Result<QuotaDecision, QuotaBackendError> {
        self.separate_calls.fetch_add(1, Ordering::SeqCst);
        Err((self.error)())
    }

    async fn acquire_request_and_route(
        &self,
        _request: &[QuotaAction],
        _route: &[QuotaAction],
        _failed: Option<u64>,
    ) -> Result<openai_server::quota::QuotaAdmission, QuotaBackendError> {
        self.combined_calls.fetch_add(1, Ordering::SeqCst);
        Err((self.error)())
    }

    async fn finish(
        &self,
        _grant: &QuotaGrant,
        _outcome: QuotaOutcome,
    ) -> Result<(), QuotaBackendError> {
        Ok(())
    }
    async fn renew(&self, _id: uuid::Uuid) -> Result<(), QuotaBackendError> {
        Ok(())
    }
    async fn cleanup(&self, _batch: u64) -> Result<(u64, u64), QuotaBackendError> {
        Ok((0, 0))
    }
}

async fn post_with_failing_admission(
    primary: &str,
    error: fn() -> QuotaBackendError,
) -> (reqwest::Response, Arc<FailingAdmissionQuota>, Vec<String>) {
    let (mut fixture, _) = counted_budget_and_rate_fixture(primary, "anthropic/unused").await;
    let quota = Arc::new(FailingAdmissionQuota::new(error));
    fixture.backend = quota.clone();
    let setup = boot_with_rate_limit(
        ApiKeyPermission::default(),
        FallbackPolicy::empty(),
        fixture,
    )
    .await;
    let response = post_stream_with_max_tokens(setup.port, primary).await;
    let opened = setup.backend.opened();
    (response, quota, opened)
}

#[tokio::test]
async fn ambiguous_admissions_fail_open_each_phase_without_replay() {
    let primary = "openai/good-ambiguous-admission";
    let (response, quota, opened) = post_with_failing_admission(primary, || {
        QuotaBackendError::unavailable("commit acknowledgement lost")
    })
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(quota.separate_calls.load(Ordering::SeqCst), 2);
    assert_eq!(quota.combined_calls.load(Ordering::SeqCst), 0);
    assert_eq!(opened, [primary]);
}

#[tokio::test]
async fn exhausted_pool_fails_open_without_queueing_route_admission() {
    let primary = "openai/good-exhausted-admission";
    let (response, quota, opened) = post_with_failing_admission(primary, || {
        QuotaBackendError::pool_exhausted("pool timed out")
    })
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        quota.separate_calls.load(Ordering::SeqCst),
        1,
        "route admission must not wait on the pool that just timed out"
    );
    assert_eq!(opened, [primary]);
}

#[tokio::test]
async fn contended_admission_fails_closed_before_dispatch() {
    let primary = "openai/good-contended-admission";
    let (response, quota, opened) = post_with_failing_admission(primary, || {
        QuotaBackendError::contended("lock wait timed out")
    })
    .await;
    assert_eq!(response.status(), 429);
    assert_eq!(response.headers()["x-chalk-error-code"], "quota_contended");
    assert_eq!(response.headers()["retry-after"], "1");
    assert_eq!(quota.separate_calls.load(Ordering::SeqCst), 1);
    assert!(opened.is_empty(), "contention must prevent dispatch");
}

#[tokio::test]
async fn postgres_request_failure_still_enforces_route_quota() {
    use openai_server::quota::PostgresQuota;
    let Some(url) = crate::support::db::test_database_url("admission-recovery") else {
        return;
    };
    openai_server::migrate::run(&url).await.unwrap();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect_with(openai_server::database::connect_options(&url).unwrap())
        .await
        .unwrap();
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let ids = ["request", "route"].map(|part| format!("recovery-{unique}-{part}"));
    let backend = Arc::new(PostgresQuota::new(pool.clone()));
    let rates = Arc::new(MemoryRateLimitPolicyStore::new("develop".to_string()));
    let hash = KeyHasher::new(b"test-pepper".to_vec()).hash(TEST_KEY);
    for (index, id) in ids.iter().enumerate() {
        sqlx::query("WITH b AS (INSERT INTO chalk_router.budget_limits(environment_id,id,scope,scope_id,window_type,window_size,limit_kind,limit_amount,enabled) VALUES('develop',$1,'api_key',$1,'calendar',INTERVAL '1 day','tokens',1000,true)) INSERT INTO chalk_router.quota_policies(id,environment_id,budget_limit_id) VALUES($1,'develop',$1)")
            .bind(id).execute(&pool).await.unwrap();
        rates
            .create(RateLimitPolicy {
                quota_policy_id: id.clone(),
                id: id.clone(),
                limit_type: "requests_per_minute".into(),
                limit_value: 1,
                burst_value: Some(1),
                connection_id: (index == 1).then(|| "openai".into()),
                models: vec![],
                scope: "api_key".into(),
                scope_id: hash.clone(),
                enabled: true,
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
    }
    // Exhaust the route independently before making the request phase fail.
    backend
        .acquire_many(
            &[QuotaAction {
                policy_id: ids[1].clone(),
                environment_id: "develop".into(),
                source: QuotaSource::RateLimit,
                algorithm: QuotaAlgorithm::TokenBucket,
                meter: QuotaMeter::Requests,
                limit_value: 1,
                burst: Some(1),
                amount: 1,
                window_start: None,
                window_end: None,
            }],
            None,
        )
        .await
        .unwrap();
    let function = format!("chalk_router.recovery_{unique}");
    let sequence = format!("chalk_router.recovery_calls_{unique}");
    let trigger = format!("recovery_{unique}");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SEQUENCE {sequence}")))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE FUNCTION {function}() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN IF NEW.policy_id=TG_ARGV[0] THEN PERFORM nextval('{sequence}'); RAISE EXCEPTION 'injected request failure'; END IF; RETURN NEW; END $$"))).execute(&pool).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE TRIGGER {trigger} AFTER UPDATE ON chalk_router.quota_states FOR EACH ROW EXECUTE FUNCTION {function}('{}')", ids[0]))).execute(&pool).await.unwrap();
    let setup = boot_with_rate_limit(
        ApiKeyPermission::default(),
        FallbackPolicy::empty(),
        QuotaFixture {
            budget_store: openai_server::budget::from_core_store(None, "develop".into()),
            rate_store: rates,
            backend,
        },
    )
    .await;
    let response = post_stream_with_max_tokens(setup.port, "openai/good-recovery").await;
    let calls: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT last_value FROM {sequence}"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    // Clear injection before assertions, even when the handler regresses.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP TRIGGER {trigger} ON chalk_router.quota_states"
    )))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP FUNCTION {function}()")))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SEQUENCE {sequence}")))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(response.status(), 429);
    assert_eq!(calls, 1, "request admission fails open once without replay");
    assert!(
        setup.backend.opened().is_empty(),
        "route denial must prevent dispatch"
    );
}
