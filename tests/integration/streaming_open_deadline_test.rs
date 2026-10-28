//! RFP-0002 §5.4: the per-attempt stream-open deadline.
//!
//! Fallback exists for provider incidents, and the common shape of an incident is
//! a *stall*, not a clean 503. The shared `reqwest::Client` is built with no
//! `timeout` and no `connect_timeout`, so without this deadline a stalled primary
//! hangs the request until the OS abandons the socket and the alternates are never
//! reached — the feature would not fire in its own headline scenario.
//!
//! **This lives in its own test binary on purpose.** `STREAM_OPEN_TIMEOUT` is a
//! `LazyLock` read from the environment once per process, so the override has to be
//! installed before anything else in the binary can force the read. One test per
//! binary makes that deterministic instead of racing sibling tests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::{middleware, routing::post, Router};

use openai_server::{
    api_keys::{ApiKeyPermission, ApiKeyStore, InMemoryApiKeyStore},
    backend::{BackendError, BackendProvider, ResolvedRoute, RouteResolution, SseByteStream},
    fallback::FallbackPolicy,
    hashing::KeyHasher,
    middleware::auth_middleware,
    types::{
        ChatCompletionRequest, ChatCompletionResponse, CreateEmbeddingRequest,
        CreateEmbeddingResponse, CreateImageRequest, CreateModerationRequest,
        CreateModerationResponse, CreateSpeechRequest, ImageResponse, ListResponse, Model,
        TranscriptionResponse,
    },
};

use crate::support::app::AppStateBuilder;

const TEST_KEY: &str = "sk-open-deadline-test";
/// Comfortably longer than the deadline below, so the stall can only be escaped by
/// the deadline firing — never by the sleep finishing first.
const STALL: Duration = Duration::from_secs(30);
const DEADLINE_MS: &str = "150";

/// Mock backend whose stream-open *stalls* for any model containing "stall" and
/// succeeds otherwise, recording each attempted open.
#[derive(Clone, Default)]
struct StallBackend {
    open_calls: Arc<Mutex<Vec<String>>>,
}

impl StallBackend {
    fn opened(&self) -> Vec<String> {
        self.open_calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl BackendProvider for StallBackend {
    async fn create_chat_completion_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<SseByteStream, BackendError> {
        self.open_calls.lock().unwrap().push(request.model.clone());
        if request.model.contains("stall") {
            // Never returns within the deadline. Dropping the timeout future
            // cancels this, which is what keeps a stalled attempt from leaking.
            tokio::time::sleep(STALL).await;
        }
        let chunk = format!(
            "data: {{\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"{m}\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"served-by:{m}\"}},\"finish_reason\":null}}]}}\n\ndata: [DONE]\n\n",
            m = request.model
        );
        Ok(Box::pin(futures::stream::iter(vec![Ok(
            bytes::Bytes::from(chunk),
        )])))
    }

    async fn create_chat_completion(
        &self,
        _request: ChatCompletionRequest,
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
        "stall-mock"
    }

    async fn resolve(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        Ok(self.route(model))
    }
    async fn resolve_strict(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        Ok(self.route(model))
    }
}

impl StallBackend {
    fn route(&self, model: &str) -> ResolvedRoute {
        let kind = model
            .split_once('/')
            .map(|(k, _)| k.to_string())
            .unwrap_or_else(|| model.to_string());
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
}

#[tokio::test]
async fn stalled_primary_open_times_out_and_the_alternate_wins() {
    // Installed before the first request, so the LazyLock resolves to this value.
    std::env::set_var("ROUTER_STREAM_OPEN_TIMEOUT_MS", DEADLINE_MS);

    let backend = Arc::new(StallBackend::default());
    let hasher = KeyHasher::new(b"test-pepper".to_vec());
    let store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(hasher.clone()));
    store
        .insert_raw(openai_server::api_keys::ApiKey {
            key_hash: hasher.hash(TEST_KEY),
            key_prefix: openai_server::hashing::key_prefix(TEST_KEY),
            description: "open deadline test key".to_string(),
            created_at: chrono::Utc::now(),
            permission: ApiKeyPermission::default(),
            labels: HashMap::new(),
            usage_pool_id: None,
            daily_token_budget: None,
        })
        .await
        .expect("seed key");

    let mut map = HashMap::new();
    map.insert(
        "openai/stall-primary".to_string(),
        vec!["openai/good-alt".to_string()],
    );

    let state = AppStateBuilder::new(backend.clone())
        .api_key_store(store)
        .fallback_policy(FallbackPolicy::from_map(map))
        .build()
        .await;

    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(openai_server::handlers::chat::create_chat_completion),
        )
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, auth_middleware));
    let base = crate::support::mock::serve(app).await;
    let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();

    let started = std::time::Instant::now();
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(TEST_KEY)
        .json(&serde_json::json!({
            "model": "openai/stall-primary",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
        }))
        .send()
        .await
        .expect("send");
    let elapsed = started.elapsed();

    assert_eq!(resp.status(), 200, "the alternate must rescue the request");
    assert_eq!(
        resp.headers()
            .get("x-fallback-model")
            .and_then(|v| v.to_str().ok()),
        Some("openai/good-alt")
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("served-by:openai/good-alt"),
        "body must come from the alternate; got: {body}"
    );
    assert_eq!(
        backend.opened(),
        vec!["openai/stall-primary", "openai/good-alt"],
        "the stalled primary was attempted and abandoned, then the alternate opened"
    );
    // Escaped via the deadline, not by outlasting the stall.
    assert!(
        elapsed < STALL,
        "request should return on the deadline (~{DEADLINE_MS}ms), not after the {STALL:?} stall; took {elapsed:?}"
    );
}
