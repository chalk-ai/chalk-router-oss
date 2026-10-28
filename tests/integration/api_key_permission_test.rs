//! Enforcement tests for `ApiKeyPermission` on the proxied request path.
//!
//! The permission allowlist (`provider` / `models`) is enforced per attempt,
//! keyed off the resolved route:
//!   * the PRIMARY is gated before dispatch — a denial is a terminal 403;
//!   * each fallback candidate is gated after its strict resolve — a denial is
//!     skipped, advancing to the next candidate (never dispatched);
//!   * an unrestricted key (`provider`/`models` both `None`) — which includes
//!     the synthetic permission minted for admin-key and federated-JWT users —
//!     is allowed everywhere.
//!
//! These drive the real chat/embeddings/images handlers behind the auth
//! middleware, with a mock backend whose `resolve`/`resolve_strict` map a model's
//! `<kind>/<rest>` prefix to a provider kind and hand back a `ResolvedRoute`
//! whose backend is the mock itself, so we can steer where a model resolves and
//! observe exactly what gets dispatched.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::{
    middleware,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use futures::StreamExt;
use openai_server::{
    api_keys::{ApiKeyConnectionScope, ApiKeyPermission, ApiKeyStore, InMemoryApiKeyStore},
    backend::{BackendError, BackendProvider, ResolvedRoute, RouteResolution, RouterBackend},
    budget::{BudgetPolicy, MemoryBudgetPolicyStore},
    fallback::FallbackPolicy,
    hashing::KeyHasher,
    middleware::auth_middleware,
    model_routes::InMemoryModelRouteStore,
    provider_connections::{
        ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore, ProviderConnectionStore,
    },
    quota::MemoryQuota,
    ratelimit::{MemoryRateLimitPolicyStore, RateLimitPolicy, RateLimitPolicyStore},
    types::{
        ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatMessage,
        CreateEmbeddingRequest, CreateEmbeddingResponse, CreateImageRequest,
        CreateModerationRequest, CreateModerationResponse, CreateSpeechRequest, Embedding,
        EmbeddingData, Image, ImageResponse, ListResponse, Model, TranscriptionResponse, Usage,
    },
    AppState,
};

use crate::support::app::AppStateBuilder;

/// Split a model id into `(provider_kind, upstream_model_id)` on the first `/`.
/// A bare name (no `/`) is its own kind with itself as the upstream id.
fn split_model(model: &str) -> (String, String) {
    match model.split_once('/') {
        Some((kind, rest)) => (kind.to_string(), rest.to_string()),
        None => (model.to_string(), model.to_string()),
    }
}

/// Mock backend: resolves a model by its `<kind>/<rest>` prefix and records the
/// models actually dispatched, so a test can assert a denied model was never
/// sent upstream. Any model containing "fail" errors on dispatch (to force the
/// fallback path). `resolve`/`resolve_strict` also count how many times a request
/// resolves, so a test can assert a request resolves exactly once.
#[derive(Clone, Default)]
struct MockBackend {
    chat_calls: Arc<Mutex<Vec<String>>>,
    embed_calls: Arc<Mutex<Vec<String>>>,
    image_calls: Arc<Mutex<Vec<String>>>,
    resolve_calls: Arc<AtomicUsize>,
    model_detail_calls: Arc<Mutex<Vec<String>>>,
    catalog: Option<Arc<ListResponse<Model>>>,
    resolution_overrides: HashMap<String, RouteResolution>,
    unresolvable_models: Vec<String>,
}

impl MockBackend {
    fn resolution(model: &str) -> RouteResolution {
        let (kind, _rest) = split_model(model);
        RouteResolution {
            provider_kind: kind.clone(),
            connection_id: None,
            connection_name: kind,
            // This fake does not strip prefixes: it dispatches (and attributes)
            // the full public model id, which is what the enforcement tests
            // assert on.
            upstream_model_id: model.to_string(),
            connection_prefix: None,
            model_route_id: None,
        }
    }

    /// A `ResolvedRoute` whose backend is a clone of this mock (sharing the same
    /// call recorders), so dispatching through it records into `chat_calls` /
    /// `embed_calls` / `image_calls` exactly as a real leaf backend would.
    fn resolved_route(&self, model: &str) -> ResolvedRoute {
        self.resolve_calls.fetch_add(1, Ordering::SeqCst);
        ResolvedRoute {
            routing: Default::default(),
            backend: Arc::new(self.clone()),
            stripped_model: model.to_string(),
            capabilities: Arc::new(
                openai_server::capabilities::resolve("openai-compatible", model, None, None)
                    .unwrap(),
            ),
            resolution: self
                .resolution_overrides
                .get(model)
                .cloned()
                .unwrap_or_else(|| Self::resolution(model)),
        }
    }
}

fn fixed_chat_response(model: &str) -> ChatCompletionResponse {
    ChatCompletionResponse {
        id: "chatcmpl-perm-test".to_string(),
        object: "chat.completion".to_string(),
        created: 1_700_000_000,
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
        if request.model.contains("fail") {
            return Err(BackendError::api_status(502, "upstream failed"));
        }
        Ok(fixed_chat_response(&request.model))
    }

    async fn create_embedding(
        &self,
        request: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        self.embed_calls.lock().unwrap().push(request.model.clone());
        if request.model.contains("fail") {
            return Err(BackendError::api_status(502, "upstream failed"));
        }
        Ok(CreateEmbeddingResponse {
            object: "list".to_string(),
            data: vec![Embedding {
                object: "embedding".to_string(),
                index: 0,
                embedding: EmbeddingData::Float(vec![0.1, 0.2, 0.3]),
            }],
            model: request.model.clone(),
            usage: Some(Usage {
                prompt_tokens: 1,
                completion_tokens: None,
                total_tokens: 1,
                ..Default::default()
            }),
        })
    }

    async fn create_image(
        &self,
        request: CreateImageRequest,
    ) -> Result<ImageResponse, BackendError> {
        self.image_calls
            .lock()
            .unwrap()
            .push(request.model.clone().unwrap_or_default());
        Ok(ImageResponse {
            created: 0,
            data: vec![Image {
                url: Some("https://example.test/img.png".to_string()),
                b64_json: None,
                revised_prompt: None,
            }],
        })
    }
    async fn create_speech(&self, _r: CreateSpeechRequest) -> Result<bytes::Bytes, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_transcription(
        &self,
        _model: String,
        _b: bytes::Bytes,
        _fname: String,
        _lang: Option<String>,
        _prompt: Option<String>,
        _fmt: Option<String>,
        _temp: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }
    async fn create_translation(
        &self,
        _model: String,
        _b: bytes::Bytes,
        _fname: String,
        _prompt: Option<String>,
        _fmt: Option<String>,
        _temp: Option<f32>,
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
        if let Some(catalog) = &self.catalog {
            return Ok(catalog.as_ref().clone());
        }
        Ok(ListResponse {
            object: "list".to_string(),
            data: ["openai/gpt-4o", "bedrock/anthropic.claude-sonnet-5"]
                .into_iter()
                .map(|id| Model {
                    id: id.to_string(),
                    object: "model".to_string(),
                    created: 0,
                    owned_by: "test".to_string(),
                    capabilities: None,
                    provider_metadata: Default::default(),
                })
                .collect(),
            first_id: Some("openai/gpt-4o".to_string()),
            last_id: Some("bedrock/anthropic.claude-sonnet-5".to_string()),
            has_more: false,
        })
    }
    async fn get_model(&self, id: &str) -> Result<Model, BackendError> {
        self.model_detail_calls.lock().unwrap().push(id.to_string());
        self.list_models()
            .await?
            .data
            .into_iter()
            .find(|model| model.id == id)
            .ok_or_else(|| BackendError::api_status(404, "Unknown model"))
    }
    fn name(&self) -> &str {
        "mock"
    }

    async fn resolve(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        if self.unresolvable_models.iter().any(|id| id == model) {
            return Err(BackendError::api_status(404, "Unknown model route"));
        }
        Ok(self.resolved_route(model))
    }
    async fn resolve_strict(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        Ok(self.resolved_route(model))
    }

    async fn resolve_default(&self) -> Result<ResolvedRoute, BackendError> {
        self.resolve_calls.fetch_add(1, Ordering::SeqCst);
        Ok(ResolvedRoute {
            routing: Default::default(),
            backend: Arc::new(self.clone()),
            stripped_model: String::new(),
            capabilities: Arc::new(
                openai_server::capabilities::resolve(
                    "openai-compatible",
                    "gpt-image-1",
                    None,
                    None,
                )
                .unwrap(),
            ),
            resolution: RouteResolution {
                provider_kind: "openai".to_string(),
                connection_id: Some("openai".to_string()),
                connection_name: "openai".to_string(),
                upstream_model_id: String::new(),
                connection_prefix: None,
                model_route_id: None,
            },
        })
    }
}

async fn build_state(
    backend: Arc<MockBackend>,
    api_key_store: Arc<dyn ApiKeyStore>,
    fallbacks: FallbackPolicy,
) -> AppState {
    AppStateBuilder::new(backend)
        .api_key_store(api_key_store)
        .fallback_policy(fallbacks)
        .build()
        .await
}

async fn with_key_quota(
    mut state: AppState,
    key_hash: String,
    budget_amount: Option<u64>,
    requests_per_minute: Option<u64>,
) -> AppState {
    let budget_store = Arc::new(MemoryBudgetPolicyStore::new("test".to_string()));
    if let Some(limit_amount) = budget_amount {
        budget_store
            .insert(BudgetPolicy {
                quota_policy_id: "endpoint-budget".to_string(),
                id: "endpoint-budget-source".to_string(),
                environment_id: "test".to_string(),
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
                limit_amount,
                enabled: true,
                created_at: chrono::Utc::now(),
            })
            .unwrap();
    }
    let rate_store = Arc::new(MemoryRateLimitPolicyStore::new("test".to_string()));
    if let Some(limit_value) = requests_per_minute {
        rate_store
            .create(RateLimitPolicy {
                quota_policy_id: "endpoint-rpm".to_string(),
                id: "endpoint-rpm-source".to_string(),
                limit_type: "requests_per_minute".to_string(),
                limit_value,
                burst_value: Some(1),
                connection_id: None,
                models: Vec::new(),
                scope: "api_key".to_string(),
                scope_id: key_hash,
                enabled: true,
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
    }
    state.budget_store = budget_store.clone();
    state.rate_limit_store = rate_store.clone();
    state.quota_resolver = Arc::new(openai_server::quota_policy::QuotaPolicyResolver::new(
        "test".to_string(),
        budget_store,
        rate_store,
    ));
    state.quota_backend = Arc::new(MemoryQuota::new());
    state
}

async fn with_key_dispatch_rpm(
    mut state: AppState,
    key_hash: String,
    models: Vec<String>,
) -> AppState {
    let budget_store = Arc::new(MemoryBudgetPolicyStore::new("test".to_string()));
    let rate_store = Arc::new(MemoryRateLimitPolicyStore::new("test".to_string()));
    rate_store
        .create(RateLimitPolicy {
            quota_policy_id: "image-dispatch-rpm".to_string(),
            id: "image-dispatch-rpm-source".to_string(),
            limit_type: "requests_per_minute".to_string(),
            limit_value: 1,
            burst_value: Some(1),
            connection_id: Some("openai".to_string()),
            models,
            scope: "api_key".to_string(),
            scope_id: key_hash,
            enabled: true,
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    state.budget_store = budget_store.clone();
    state.rate_limit_store = rate_store.clone();
    state.quota_resolver = Arc::new(openai_server::quota_policy::QuotaPolicyResolver::new(
        "test".to_string(),
        budget_store,
        rate_store,
    ));
    state.quota_backend = Arc::new(MemoryQuota::new());
    state
}

async fn boot(state: AppState) -> u16 {
    let app = Router::new()
        .route(
            "/v1/files",
            post(openai_server::handlers::files::upload_file),
        )
        .route(
            "/v1/models",
            get(openai_server::handlers::models::list_models),
        )
        .route(
            "/v1/models/projections/vscode",
            get(openai_server::handlers::models::project_models_for_vscode),
        )
        .route(
            "/v1/models/:model_id",
            get(openai_server::handlers::models::get_model),
        )
        .route(
            "/v1/chat/completions",
            post(openai_server::handlers::chat::create_chat_completion),
        )
        .route(
            "/v1/embeddings",
            post(openai_server::handlers::embeddings::create_embedding),
        )
        .route(
            "/v1/images/generations",
            post(openai_server::handlers::images::create_image),
        )
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, auth_middleware));
    let base = crate::support::mock::serve(app).await;
    base.rsplit(':').next().unwrap().parse().unwrap()
}

fn perm(provider: Option<&str>, models: Option<&[&str]>) -> ApiKeyPermission {
    ApiKeyPermission {
        provider: provider.map(str::to_string),
        models: models.map(|m| m.iter().map(|s| s.to_string()).collect()),
        ..Default::default()
    }
}

/// Create a key with `permission` in a fresh in-memory store and return
/// (store, raw_key) so the raw secret can be presented as a bearer token.
async fn key_with(permission: ApiKeyPermission) -> (Arc<dyn ApiKeyStore>, String) {
    let store = Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
        b"test-pepper".to_vec(),
    )));
    let created = store
        .create("perm-test".to_string(), permission, HashMap::new(), None)
        .await
        .expect("create key");
    (store as Arc<dyn ApiKeyStore>, created.raw_key)
}

async fn chat_body(model: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "hi"}],
    })
}

// ---------------------------------------------------------------------------

async fn canonical_alias_app(
    scope: &str,
    selector: &str,
    limit_type: &str,
) -> (u16, String, Arc<AtomicUsize>, Arc<tokio::sync::Notify>) {
    canonical_alias_app_for_model(scope, selector, limit_type, "gpt-6-luna").await
}

async fn canonical_alias_app_for_model(
    scope: &str,
    selector: &str,
    limit_type: &str,
    model: &str,
) -> (u16, String, Arc<AtomicUsize>, Arc<tokio::sync::Notify>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let upstream_calls = calls.clone();
    let upstream_release = release.clone();
    let embedding_calls = calls.clone();
    let expected_model = model.to_string();
    let upstream = crate::support::mock::serve(Router::new().route(
        "/chat/completions",
        post(move |axum::Json(body): axum::Json<serde_json::Value>| {
            let calls = upstream_calls.clone();
            let release = upstream_release.clone();
            let expected_model = expected_model.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(body["model"], expected_model);
                if body["stream"] == true {
                    let first = bytes::Bytes::from_static(b"data: {\"id\":\"chatcmpl-alias\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-6-luna\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\n");
                    let stream = futures::stream::once(async move { Ok::<_, std::io::Error>(first) })
                        .chain(futures::stream::once(async move {
                            release.notified().await;
                            Ok(bytes::Bytes::from_static(b"data: {\"id\":\"chatcmpl-alias\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"gpt-6-luna\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\ndata: [DONE]\n\n"))
                        }));
                    axum::http::Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(axum::body::Body::from_stream(stream)).unwrap().into_response()
                } else {
                    axum::Json(fixed_chat_response("gpt-6-luna")).into_response()
                }
            }
        }),
    ).route("/embeddings", post(move |axum::Json(body): axum::Json<serde_json::Value>| {
        let calls = embedding_calls.clone();
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            axum::Json(serde_json::json!({"object":"list", "model":body["model"], "data":[{"object":"embedding", "index":0, "embedding":[0.5]}], "usage":{"prompt_tokens":1,"total_tokens":1}}))
        }
    })))
    .await;
    let connections = Arc::new(InMemoryProviderConnectionStore::new());
    let connection = connections
        .create(ConnectionInput {
            name: "openai".into(),
            provider_kind: "openai".into(),
            prefix: Some("openai/".into()),
            base_url: Some(upstream),
            api_key: "mock".into(),
            exposure: ExposurePolicy::Unlisted,
            routing_enabled: true,
            declared_capabilities: None,
        })
        .await
        .unwrap();
    let routes = Arc::new(InMemoryModelRouteStore::new());
    let backend = Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        connections.clone(),
        routes.clone(),
    ));
    let store = Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
        b"test-pepper".to_vec(),
    )));
    let created = store
        .create(
            "alias-test".into(),
            perm(None, Some(&[&format!("openai/{model}")])),
            HashMap::new(),
            (scope == "usage_pool").then(|| "alias-pool".into()),
        )
        .await
        .unwrap();
    let raw = created.raw_key;
    let key_hash = KeyHasher::new(b"test-pepper".to_vec()).hash(&raw);
    let mut state = AppStateBuilder::new(backend)
        .api_key_store(store)
        .provider_connection_store(connections)
        .model_route_store(routes)
        .build()
        .await;
    let budget_store = Arc::new(MemoryBudgetPolicyStore::new("test".into()));
    let rate_store = Arc::new(MemoryRateLimitPolicyStore::new("test".into()));
    rate_store
        .create(RateLimitPolicy {
            quota_policy_id: "alias-policy".into(),
            id: "alias-policy-source".into(),
            limit_type: limit_type.into(),
            limit_value: 1,
            burst_value: (limit_type != "concurrent_requests").then_some(1),
            connection_id: Some(connection.id),
            models: vec![selector.into()],
            scope: scope.into(),
            scope_id: if scope == "usage_pool" {
                "alias-pool".into()
            } else {
                key_hash
            },
            enabled: true,
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    state.budget_store = budget_store.clone();
    state.rate_limit_store = rate_store.clone();
    state.quota_resolver = Arc::new(openai_server::quota_policy::QuotaPolicyResolver::new(
        "test".into(),
        budget_store,
        rate_store,
    ));
    state.quota_backend = Arc::new(MemoryQuota::new());
    (boot(state).await, raw, calls, release)
}

#[tokio::test]
async fn canonical_bare_model_cannot_bypass_prefixed_dispatch_rpm() {
    let (port, raw, calls, _) =
        canonical_alias_app("api_key", "openai/gpt-6-luna", "requests_per_minute").await;
    let client = reqwest::Client::new();
    let mut statuses = Vec::new();
    for model in [
        "openai/gpt-6-luna",
        "openai/gpt-6-luna",
        "gpt-6-luna",
        "gpt-6-luna",
    ] {
        let response = client
            .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .bearer_auth(&raw)
            .json(&chat_body(model).await)
            .send()
            .await
            .unwrap();
        statuses.push(response.status().as_u16());
        response.bytes().await.unwrap();
    }
    assert_eq!(statuses, [200, 429, 429, 429]);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn canonical_prefixed_model_cannot_bypass_bare_pool_dispatch_rpm() {
    let (port, raw, calls, _) =
        canonical_alias_app("usage_pool", "gpt-6-luna", "requests_per_minute").await;
    let client = reqwest::Client::new();
    for (model, expected) in [("gpt-6-luna", 200), ("openai/gpt-6-luna", 429)] {
        let response = client
            .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .bearer_auth(&raw)
            .json(&chat_body(model).await)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected);
        response.bytes().await.unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn canonical_embedding_aliases_share_single_route_rpm() {
    let (port, raw, calls, _) = canonical_alias_app_for_model(
        "api_key",
        "text-embedding-3-small",
        "requests_per_minute",
        "text-embedding-3-small",
    )
    .await;
    let client = reqwest::Client::new();
    for (model, expected) in [
        ("text-embedding-3-small", 200),
        ("openai/text-embedding-3-small", 429),
    ] {
        let response = client
            .post(format!("http://127.0.0.1:{port}/v1/embeddings"))
            .bearer_auth(&raw)
            .json(&serde_json::json!({"model":model,"input":"hi"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected);
        response.bytes().await.unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn canonical_aliases_both_reject_matching_pool_tpm_before_dispatch() {
    let (port, raw, calls, _) =
        canonical_alias_app("usage_pool", "openai/gpt-6-luna", "tokens_per_minute").await;
    let client = reqwest::Client::new();
    for model in ["gpt-6-luna", "openai/gpt-6-luna"] {
        let response = client.post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .bearer_auth(&raw).json(&serde_json::json!({"model":model,"messages":[{"role":"user","content":"hi"}],"max_tokens":4})).send().await.unwrap();
        assert_eq!(response.status().as_u16(), 429);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn canonical_model_aliases_share_streaming_concurrency_and_release_permits() {
    let (port, raw, calls, release) =
        canonical_alias_app("usage_pool", "gpt-6-luna", "concurrent_requests").await;
    let client = reqwest::Client::new();
    let stream_request = |model| serde_json::json!({"model":model, "messages":[{"role":"user","content":"hi"}], "stream":true});
    let first = client
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(&raw)
        .json(&stream_request("openai/gpt-6-luna"))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status().as_u16(), 200);
    let blocked = client
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(&raw)
        .json(&stream_request("gpt-6-luna"))
        .send()
        .await
        .unwrap();
    assert_eq!(blocked.status().as_u16(), 429);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    release.notify_one();
    assert!(first.text().await.unwrap().contains("[DONE]"));
    let second = client
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(&raw)
        .json(&stream_request("gpt-6-luna"))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status().as_u16(), 200);
    release.notify_one();
    second.bytes().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn model_catalog_respects_key_permissions() {
    for (permission, expected) in [
        (perm(None, Some(&["openai/gpt-4o"])), vec!["openai/gpt-4o"]),
        (
            perm(Some("bedrock"), None),
            vec!["bedrock/anthropic.claude-sonnet-5"],
        ),
        (perm(Some("bedrock"), Some(&["openai/gpt-4o"])), vec![]),
        (perm(None, Some(&[])), vec![]),
    ] {
        let backend = Arc::new(MockBackend::default());
        let (store, raw) = key_with(permission).await;
        let port = boot(build_state(backend.clone(), store, FallbackPolicy::empty()).await).await;
        let response = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/v1/models"))
            .bearer_auth(raw)
            .send()
            .await
            .expect("list models");
        assert_eq!(response.status(), 200);
        let catalog: ListResponse<Model> = response.json().await.expect("model catalog");
        let ids: Vec<_> = catalog.data.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, expected);
        assert_eq!(catalog.first_id.as_deref(), expected.first().copied());
        assert_eq!(catalog.last_id.as_deref(), expected.last().copied());
        assert!(backend.chat_calls.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn unrestricted_model_catalog_does_not_resolve_routes() {
    let backend = Arc::new(MockBackend::default());
    let (store, raw) = key_with(perm(None, None)).await;
    let port = boot(build_state(backend.clone(), store, FallbackPolicy::empty()).await).await;
    let response = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/v1/models"))
        .bearer_auth(raw)
        .send()
        .await
        .expect("list models");
    assert_eq!(response.status(), 200);
    let catalog: ListResponse<Model> = response.json().await.expect("model catalog");
    assert_eq!(catalog.data.len(), 2);
    assert_eq!(backend.resolve_calls.load(Ordering::SeqCst), 0);
}

/// An alias on a stored Bedrock connection, sharing a catalog with an OpenAI
/// model. The public alias and upstream id deliberately differ.
async fn discovery_backend() -> Arc<MockBackend> {
    let mut backend = MockBackend::default();
    let mut catalog = backend.list_models().await.unwrap();
    catalog.data[1].id = "chalk/assistant".to_string();
    catalog.last_id = Some("chalk/assistant".to_string());
    backend.catalog = Some(Arc::new(catalog));
    backend.resolution_overrides.insert(
        "chalk/assistant".to_string(),
        RouteResolution {
            provider_kind: "bedrock".to_string(),
            connection_id: Some("conn-bedrock".to_string()),
            connection_name: "Production Claude".to_string(),
            upstream_model_id: "anthropic.claude-sonnet-5".to_string(),
            connection_prefix: None,
            model_route_id: Some("route-assistant".to_string()),
        },
    );
    Arc::new(backend)
}

#[tokio::test]
async fn discovery_formats_filter_aliases_per_key_without_changing_shared_catalog() {
    let backend = discovery_backend().await;
    let (store, openai_key) = key_with(perm(None, Some(&["openai/gpt-4o"]))).await;
    let scoped = store
        .create(
            "scoped".to_string(),
            ApiKeyPermission {
                route_ids: Some(vec!["route-assistant".to_string()]),
                ..Default::default()
            },
            HashMap::new(),
            None,
        )
        .await
        .unwrap();
    let unrestricted = store
        .create(
            "unrestricted".to_string(),
            perm(None, None),
            HashMap::new(),
            None,
        )
        .await
        .unwrap();
    let port = boot(build_state(backend.clone(), store, FallbackPolicy::empty()).await).await;
    let client = reqwest::Client::new();
    for (key, expected) in [
        (openai_key.as_str(), vec!["openai/gpt-4o"]),
        (scoped.raw_key.as_str(), vec!["chalk/assistant"]),
        (
            unrestricted.raw_key.as_str(),
            vec!["openai/gpt-4o", "chalk/assistant"],
        ),
        (openai_key.as_str(), vec!["openai/gpt-4o"]),
    ] {
        for (path, field) in [
            ("/v1/models", "data"),
            (
                "/v1/models/projections/vscode?url=/v1/router/responses",
                "models",
            ),
        ] {
            let response = client
                .get(format!("http://127.0.0.1:{port}{path}"))
                .bearer_auth(key)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let body: serde_json::Value = response.json().await.unwrap();
            let models = body[field].as_array().unwrap();
            let ids: Vec<_> = models
                .iter()
                .map(|model| model["id"].as_str().unwrap())
                .collect();
            assert_eq!(ids, expected, "{path}");
            if field == "models" {
                for model in models {
                    assert_eq!(model["url"], "/v1/router/responses");
                    assert!(model["toolCalling"].is_boolean());
                    assert!(model["vision"].is_boolean());
                }
            }
        }
    }
    assert_eq!(backend.catalog.as_ref().unwrap().data.len(), 2);
    assert!(backend.model_detail_calls.lock().unwrap().is_empty());
    assert!(backend.chat_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn model_lookup_checks_resolved_permissions_before_fetching_details() {
    let routes = |ids: &[&str]| ApiKeyPermission {
        route_ids: Some(ids.iter().map(|id| id.to_string()).collect()),
        ..Default::default()
    };
    for (permission, allowed) in [
        (perm(None, None), true),
        (routes(&["route-assistant"]), true),
        (routes(&["route-other"]), false),
        (routes(&[]), false),
        // Connection grants never reach a model route, even one listed by
        // public id or served by the granted connection.
        (perm(Some("bedrock"), Some(&["chalk/assistant"])), false),
        (perm(None, Some(&["anthropic.claude-sonnet-5"])), false),
        (
            ApiKeyPermission {
                connection_id: Some("conn-bedrock".to_string()),
                ..Default::default()
            },
            false,
        ),
        (
            ApiKeyPermission {
                connection_scopes: Some(vec![ApiKeyConnectionScope {
                    connection_id: "conn-bedrock".to_string(),
                    models: Vec::new(),
                }]),
                ..Default::default()
            },
            false,
        ),
    ] {
        let backend = discovery_backend().await;
        let (store, raw) = key_with(permission).await;
        let port = boot(build_state(backend.clone(), store, FallbackPolicy::empty()).await).await;
        let response = reqwest::Client::new()
            .get(format!(
                "http://127.0.0.1:{port}/v1/models/chalk%2Fassistant"
            ))
            .bearer_auth(raw)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), if allowed { 200 } else { 403 });
        let body: serde_json::Value = response.json().await.unwrap();
        if allowed {
            assert_eq!(body["id"], "chalk/assistant");
            assert_eq!(
                backend.model_detail_calls.lock().unwrap().as_slice(),
                ["chalk/assistant"]
            );
        } else {
            assert_eq!(body["error"]["type"], "permission_error");
            assert_eq!(body["error"]["code"], "access_denied");
            assert!(backend.model_detail_calls.lock().unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn discovery_omits_unresolvable_models_and_lookup_returns_resolution_error() {
    let mut backend = discovery_backend().await.as_ref().clone();
    backend
        .unresolvable_models
        .push("chalk/assistant".to_string());
    let backend = Arc::new(backend);
    let (store, raw) = key_with(perm(None, Some(&["openai/gpt-4o", "chalk/assistant"]))).await;
    let port = boot(build_state(backend.clone(), store, FallbackPolicy::empty()).await).await;
    let client = reqwest::Client::new();
    for (path, field) in [
        ("/v1/models", "data"),
        ("/v1/models/projections/vscode", "models"),
    ] {
        let response = client
            .get(format!("http://127.0.0.1:{port}{path}"))
            .bearer_auth(&raw)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: serde_json::Value = response.json().await.unwrap();
        let models = body[field].as_array().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["id"], "openai/gpt-4o");
    }
    let response = client
        .get(format!(
            "http://127.0.0.1:{port}/v1/models/chalk%2Fassistant"
        ))
        .bearer_auth(raw)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert!(backend.model_detail_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn discovery_endpoints_require_authentication() {
    let backend = discovery_backend().await;
    let (store, _) = key_with(perm(None, None)).await;
    let port = boot(build_state(backend.clone(), store, FallbackPolicy::empty()).await).await;
    for path in [
        "/v1/models",
        "/v1/models/projections/vscode",
        "/v1/models/chalk%2Fassistant",
    ] {
        let response = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
    }
    assert!(backend.model_detail_calls.lock().unwrap().is_empty());
    assert_eq!(backend.resolve_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unrestricted_key_is_allowed_everywhere() {
    let backend = Arc::new(MockBackend::default());
    // Unrestricted == the same synthetic permission admin-key / federated-JWT
    // users are minted with, so this also covers those paths.
    let (store, raw) = key_with(perm(None, None)).await;
    let port = boot(build_state(backend.clone(), store, FallbackPolicy::empty()).await).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(&raw)
        .json(&chat_body("openai/gpt-4o").await)
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        backend.chat_calls.lock().unwrap().as_slice(),
        ["openai/gpt-4o"]
    );
}

#[tokio::test]
async fn provider_scoped_key_denies_primary_before_dispatch() {
    let backend = Arc::new(MockBackend::default());
    let (store, raw) = key_with(perm(Some("anthropic"), None)).await;
    let port = boot(build_state(backend.clone(), store, FallbackPolicy::empty()).await).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(&raw)
        .json(&chat_body("openai/gpt-4o").await)
        .send()
        .await
        .expect("send");
    // Terminal 403, and the disallowed model was never dispatched upstream.
    assert_eq!(resp.status(), 403);
    assert!(
        backend.chat_calls.lock().unwrap().is_empty(),
        "denied primary must not dispatch"
    );
}

#[tokio::test]
async fn provider_scoped_key_skips_denied_fallback_and_uses_permitted_one() {
    let backend = Arc::new(MockBackend::default());
    let (store, raw) = key_with(perm(Some("anthropic"), None)).await;
    // Primary is permitted (anthropic) but fails; the first fallback is a
    // different provider the key can't reach (must be skipped), the second is a
    // permitted anthropic alternate (must succeed).
    let mut map = HashMap::new();
    map.insert(
        "anthropic/fail-primary".to_string(),
        vec![
            "openai/gpt-4o".to_string(),
            "anthropic/claude-ok".to_string(),
        ],
    );
    let port = boot(build_state(backend.clone(), store, FallbackPolicy::from_map(map)).await).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(&raw)
        .json(&chat_body("anthropic/fail-primary").await)
        .send()
        .await
        .expect("send");

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-fallback-model")
            .and_then(|v| v.to_str().ok()),
        Some("anthropic/claude-ok"),
    );
    let calls = backend.chat_calls.lock().unwrap().clone();
    // Primary attempted, denied openai alternate skipped, permitted alternate dispatched.
    assert!(calls.contains(&"anthropic/fail-primary".to_string()));
    assert!(calls.contains(&"anthropic/claude-ok".to_string()));
    assert!(
        !calls.contains(&"openai/gpt-4o".to_string()),
        "denied fallback must not dispatch"
    );
}

#[tokio::test]
async fn model_allowlist_filters_both_primary_and_fallback() {
    let backend = Arc::new(MockBackend::default());
    // Only these two public ids are allowed. Primary is on the list (allowed)
    // but fails; the off-list fallback is skipped; the on-list fallback wins.
    let (store, raw) = key_with(perm(
        None,
        Some(&["anthropic/fail-primary", "anthropic/claude-ok"]),
    ))
    .await;
    let mut map = HashMap::new();
    map.insert(
        "anthropic/fail-primary".to_string(),
        vec![
            "openai/gpt-4o".to_string(),
            "anthropic/claude-ok".to_string(),
        ],
    );
    let port = boot(build_state(backend.clone(), store, FallbackPolicy::from_map(map)).await).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(&raw)
        .json(&chat_body("anthropic/fail-primary").await)
        .send()
        .await
        .expect("send");

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("x-fallback-model")
            .and_then(|v| v.to_str().ok()),
        Some("anthropic/claude-ok"),
    );
    assert!(!backend
        .chat_calls
        .lock()
        .unwrap()
        .contains(&"openai/gpt-4o".to_string()));
}

#[tokio::test]
async fn streaming_primary_denied_before_stream_opens() {
    let backend = Arc::new(MockBackend::default());
    let (store, raw) = key_with(perm(Some("anthropic"), None)).await;
    let port = boot(build_state(backend.clone(), store, FallbackPolicy::empty()).await).await;

    let mut body = chat_body("openai/gpt-4o").await;
    body["stream"] = serde_json::json!(true);
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(&raw)
        .json(&body)
        .send()
        .await
        .expect("send");
    // A real non-2xx (not a 200 event-stream), and nothing dispatched.
    assert_eq!(resp.status(), 403);
    assert!(backend.chat_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn route_is_resolved_exactly_once_per_request() {
    // A single request must resolve the route exactly once (no re-resolve for
    // dispatch or attribution) and dispatch to exactly that resolved backend.
    let backend = Arc::new(MockBackend::default());
    let (store, raw) = key_with(perm(None, None)).await;
    let port = boot(build_state(backend.clone(), store, FallbackPolicy::empty()).await).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(&raw)
        .json(&chat_body("openai/gpt-4o").await)
        .send()
        .await
        .expect("send");

    assert_eq!(resp.status(), 200);
    assert_eq!(
        backend.resolve_calls.load(Ordering::SeqCst),
        1,
        "request must resolve the route exactly once"
    );
    // Dispatched once, to the resolved backend.
    assert_eq!(
        backend.chat_calls.lock().unwrap().as_slice(),
        ["openai/gpt-4o"]
    );
}

#[tokio::test]
async fn image_generation_respects_permission() {
    let backend = Arc::new(MockBackend::default());
    let (store, raw) = key_with(perm(Some("anthropic"), None)).await;
    let port = boot(build_state(backend.clone(), store, FallbackPolicy::empty()).await).await;
    let client = reqwest::Client::new();

    // Denied: an openai image model under an anthropic-scoped key.
    let denied = client
        .post(format!("http://127.0.0.1:{port}/v1/images/generations"))
        .bearer_auth(&raw)
        .json(&serde_json::json!({"prompt": "a cat", "model": "openai/dall-e-3"}))
        .send()
        .await
        .expect("send");
    assert_eq!(denied.status(), 403);
    assert!(
        backend.image_calls.lock().unwrap().is_empty(),
        "denied image model must not dispatch"
    );

    // Allowed: an anthropic image model under the same key.
    let ok = client
        .post(format!("http://127.0.0.1:{port}/v1/images/generations"))
        .bearer_auth(&raw)
        .json(&serde_json::json!({"prompt": "a cat", "model": "anthropic/img-gen"}))
        .send()
        .await
        .expect("send");
    assert_eq!(ok.status(), 200);
    assert_eq!(
        backend.image_calls.lock().unwrap().as_slice(),
        ["anthropic/img-gen"]
    );
}

#[tokio::test]
async fn embeddings_respect_permission() {
    let backend = Arc::new(MockBackend::default());
    let (store, raw) = key_with(perm(Some("anthropic"), None)).await;
    let port = boot(build_state(backend.clone(), store, FallbackPolicy::empty()).await).await;
    let client = reqwest::Client::new();

    // Denied: openai embedding under an anthropic-scoped key.
    let denied = client
        .post(format!("http://127.0.0.1:{port}/v1/embeddings"))
        .bearer_auth(&raw)
        .json(&serde_json::json!({"model": "openai/text-embed", "input": "hello"}))
        .send()
        .await
        .expect("send");
    assert_eq!(denied.status(), 403);
    assert!(
        backend.embed_calls.lock().unwrap().is_empty(),
        "denied embedding must not dispatch"
    );

    // Allowed: anthropic embedding under the same key.
    let ok = client
        .post(format!("http://127.0.0.1:{port}/v1/embeddings"))
        .bearer_auth(&raw)
        .json(&serde_json::json!({"model": "anthropic/embed", "input": "hello"}))
        .send()
        .await
        .expect("send");
    assert_eq!(ok.status(), 200);
    assert_eq!(
        backend.embed_calls.lock().unwrap().as_slice(),
        ["anthropic/embed"]
    );
}

#[tokio::test]
async fn token_budget_blocks_embeddings_before_dispatch() {
    let backend = Arc::new(MockBackend::default());
    let (store, raw) = key_with(ApiKeyPermission::default()).await;
    let key_hash = KeyHasher::new(b"test-pepper".to_vec()).hash(&raw);
    let state = with_key_quota(
        build_state(backend.clone(), store, FallbackPolicy::empty()).await,
        key_hash,
        Some(1),
        None,
    )
    .await;
    let port = boot(state).await;

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/embeddings"))
        .bearer_auth(&raw)
        .json(&serde_json::json!({"model": "openai/text-embed", "input": "hello"}))
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), 429);
    assert!(backend.embed_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn request_only_images_skip_token_budgets_but_consume_global_rpm() {
    let backend = Arc::new(MockBackend::default());
    let (store, raw) = key_with(ApiKeyPermission::default()).await;
    let key_hash = KeyHasher::new(b"test-pepper".to_vec()).hash(&raw);
    let state = with_key_quota(
        build_state(backend.clone(), store, FallbackPolicy::empty()).await,
        key_hash,
        Some(1),
        Some(1),
    )
    .await;
    let port = boot(state).await;
    let client = reqwest::Client::new();
    let request = || {
        client
            .post(format!("http://127.0.0.1:{port}/v1/images/generations"))
            .bearer_auth(&raw)
            .json(&serde_json::json!({"prompt": "a cat", "model": "openai/image"}))
    };

    assert_eq!(request().send().await.unwrap().status(), 200);
    assert_eq!(request().send().await.unwrap().status(), 429);
    assert_eq!(backend.image_calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn default_image_requires_model_when_model_scoped_rate_limit_applies() {
    let backend = Arc::new(MockBackend::default());
    let (store, raw) = key_with(ApiKeyPermission::default()).await;
    let key_hash = KeyHasher::new(b"test-pepper".to_vec()).hash(&raw);
    let state = with_key_dispatch_rpm(
        build_state(backend.clone(), store, FallbackPolicy::empty()).await,
        key_hash,
        vec!["openai/gpt-image-1".to_string()],
    )
    .await;
    let port = boot(state).await;

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/images/generations"))
        .bearer_auth(&raw)
        .json(&serde_json::json!({"prompt": "a cat"}))
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), 400);
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("model is required when model-scoped rate limits apply"));
    assert!(backend.image_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn default_image_consumes_connection_wide_rate_limit() {
    let backend = Arc::new(MockBackend::default());
    let (store, raw) = key_with(ApiKeyPermission::default()).await;
    let key_hash = KeyHasher::new(b"test-pepper".to_vec()).hash(&raw);
    let state = with_key_dispatch_rpm(
        build_state(backend.clone(), store, FallbackPolicy::empty()).await,
        key_hash,
        Vec::new(),
    )
    .await;
    let port = boot(state).await;
    let client = reqwest::Client::new();
    let request = || {
        client
            .post(format!("http://127.0.0.1:{port}/v1/images/generations"))
            .bearer_auth(&raw)
            .json(&serde_json::json!({"prompt": "a cat"}))
    };

    assert_eq!(request().send().await.unwrap().status(), 200);
    assert_eq!(request().send().await.unwrap().status(), 429);
    assert_eq!(backend.image_calls.lock().unwrap().as_slice(), [""]);
}

/// The model-detail endpoint keeps the backend's status for an unknown model
/// instead of reporting a router failure.
#[tokio::test]
async fn unknown_model_detail_keeps_the_backend_status() {
    let backend = Arc::new(MockBackend::default());
    let (store, raw) = key_with(perm(None, None)).await;
    let port = boot(build_state(backend, store, FallbackPolicy::empty()).await).await;
    let response = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{port}/v1/models/openai%2Fdoes-not-exist"
        ))
        .bearer_auth(raw)
        .send()
        .await
        .expect("model detail");
    assert_eq!(response.status(), 404);
    let body: serde_json::Value = response.json().await.expect("error body");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Unknown model"),
        "{body}"
    );
}

#[tokio::test]
async fn inference_permissions_and_admin_credentials_never_enable_file_uploads() {
    for permission in [
        perm(None, None),
        perm(None, Some(&[])),
        perm(Some("openai"), Some(&["openai/model"])),
    ] {
        let (store, key) = key_with(permission).await;
        let mut state = build_state(
            Arc::new(MockBackend::default()),
            store,
            FallbackPolicy::default(),
        )
        .await;
        state.admin_key = Some("file-test-admin".to_string());
        let port = boot(state).await;
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/v1/files");
        for credential in [&key, "file-test-admin"] {
            let response = client.post(&url).bearer_auth(credential)
                .header("content-type", "multipart/form-data; boundary=fixture")
                .body("--fixture\r\nContent-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\n\r\nsynthetic data\r\n--fixture--\r\n")
                .send().await.unwrap();
            assert_eq!(response.status(), 501);
            let json: serde_json::Value = response.json().await.unwrap();
            assert_eq!(json["error"]["type"], "not_implemented");
        }
        assert_eq!(client.post(&url).send().await.unwrap().status(), 401);
        assert_eq!(
            client
                .post(&url)
                .bearer_auth("invalid")
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
    }
}
