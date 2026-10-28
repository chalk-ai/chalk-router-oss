//! Router Decisions and TypeSafe System One coverage across the shared request
//! pipeline and both supported upstream transports.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{middleware, routing::get, routing::post, Json, Router};
use http_body_util::BodyExt;
use tower::ServiceExt;

use openai_server::api_keys::{ApiKey, ApiKeyPermission, ApiKeyStore, InMemoryApiKeyStore};
use openai_server::backend::{
    BackendError, BackendProvider, ResolvedRoute, RouteResolution, RouterBackend,
};
use openai_server::capabilities::{
    DeliveryMode, ModelSurface, Protocol, ProtocolOperation, Support,
};
use openai_server::decisions::DecisionsRequest;
use openai_server::fallback::FallbackPolicy;
use openai_server::handlers;
use openai_server::hashing::KeyHasher;
use openai_server::middleware::auth_middleware;
use openai_server::model_routes::InMemoryModelRouteStore;
use openai_server::provider_connections::{
    ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore, ProviderConnectionStore,
};
use openai_server::quota::{
    MemoryQuota, QuotaAction, QuotaBackend, QuotaBackendError, QuotaDecision, QuotaGrant,
    QuotaOutcome,
};
use openai_server::quota_policy::QuotaPolicyResolver;
use openai_server::ratelimit::{MemoryRateLimitPolicyStore, RateLimitPolicy, RateLimitPolicyStore};
use openai_server::types::{ListResponse, Model};

use crate::support::app::{AppStateBuilder, TEST_API_KEY};

fn decision_body(model: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "state": {"ticket": "Checkout is blank"},
        "questions": {
            "team": {
                "type": "choice",
                "instructions": "Which team owns this?",
                "criteria": {
                    "frontend": "Rendering and browser issues",
                    "payments": "Billing and checkout issues"
                }
            }
        },
        "session_id": "workflow-1"
    })
}

fn system_one_body(model: &str) -> serde_json::Value {
    let mut body = decision_body(model);
    body.as_object_mut().unwrap().remove("session_id");
    body
}

fn request(path: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {TEST_API_KEY}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

async fn response_body(response: Response) -> bytes::Bytes {
    response
        .into_body()
        .collect()
        .await
        .expect("response body")
        .to_bytes()
}

fn app(state: openai_server::AppState) -> Router {
    Router::new()
        .route(
            "/v1/alpha/decisions",
            post(handlers::decisions::create_decision),
        )
        .route(
            "/v1/systemone",
            post(handlers::decisions::create_system_one),
        )
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, auth_middleware))
}

#[derive(Default)]
struct UpstreamState {
    requests: Mutex<Vec<serde_json::Value>>,
    authorization: Mutex<Vec<Option<String>>>,
    status: Mutex<Option<StatusCode>>,
    response_model: Mutex<String>,
}

impl UpstreamState {
    fn successful(model: &str) -> Arc<Self> {
        Arc::new(Self {
            response_model: Mutex::new(model.to_string()),
            ..Default::default()
        })
    }

    fn failing(status: StatusCode) -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(Some(status)),
            ..Default::default()
        })
    }
}

async fn decision_upstream(
    State(state): State<Arc<UpstreamState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    state.requests.lock().unwrap().push(body);
    state.authorization.lock().unwrap().push(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
    );
    if let Some(status) = *state.status.lock().unwrap() {
        return (
            status,
            [(header::CONTENT_TYPE, "application/problem+json")],
            r#"{"error":{"message":"decision backend unavailable"}}"#,
        )
            .into_response();
    }
    Json(serde_json::json!({
        "id": "decision-1",
        "model": state.response_model.lock().unwrap().clone(),
        "answers": {
            "team": {"type": "choice", "choice": "payments", "confidence": 0.9}
        },
        "usage": {"input_tokens": 1_000_000, "output_tokens": 70}
    }))
    .into_response()
}

async fn typesafe_models_upstream() -> impl IntoResponse {
    Json(serde_json::json!({
        "models": [
            {
                "name": "jev-latest",
                "description": "Latest stable Jev",
                "release_date": "2026-09-17"
            },
            {
                "name": "future-system-one",
                "description": "Future account-visible System One model",
                "release_date": "2026-10-01"
            }
        ]
    }))
}

async fn openrouter_models_upstream() -> impl IntoResponse {
    Json(serde_json::json!({
        "object": "list",
        "data": [{
            "id": "typesafe/jev-1.13",
            "object": "model",
            "owned_by": "typesafe"
        }]
    }))
}

async fn add_connection(
    connections: &Arc<InMemoryProviderConnectionStore>,
    provider_kind: &str,
    base_url: String,
    api_key: &str,
    exposure: ExposurePolicy,
) {
    connections
        .create(ConnectionInput {
            name: format!("{provider_kind}-primary"),
            provider_kind: provider_kind.to_string(),
            base_url: Some(base_url),
            prefix: Some(format!("{provider_kind}/")),
            exposure,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: api_key.to_string(),
        })
        .await
        .unwrap();
}

fn router_backend(connections: Arc<InMemoryProviderConnectionStore>) -> Arc<RouterBackend> {
    Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        connections,
        Arc::new(InMemoryModelRouteStore::new()),
    ))
}

#[derive(Default)]
struct FinishRecordingQuota {
    inner: MemoryQuota,
    finishes: AtomicUsize,
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
        self.inner.finish(grant, outcome).await
    }

    async fn renew(&self, permit_id: uuid::Uuid) -> Result<(), QuotaBackendError> {
        self.inner.renew(permit_id).await
    }

    async fn cleanup(&self, batch_size: u64) -> Result<(u64, u64), QuotaBackendError> {
        self.inner.cleanup(batch_size).await
    }
}

#[tokio::test]
async fn alpha_decisions_uses_typesafe_transport_catalog_and_quota_once() {
    let upstream_state = UpstreamState::successful("jev-1.13.0");
    let upstream = Router::new()
        .route("/v1/systemone", post(decision_upstream))
        .route("/v1/models", get(typesafe_models_upstream))
        .with_state(upstream_state.clone());
    let upstream_base = crate::support::mock::serve(upstream).await;

    let connections = Arc::new(InMemoryProviderConnectionStore::new());
    add_connection(
        &connections,
        "typesafe",
        format!("{upstream_base}/v1"),
        "typesafe-secret",
        ExposurePolicy::Dynamic,
    )
    .await;
    let router = router_backend(connections);

    let catalog = router.list_models().await.unwrap();
    assert_eq!(
        catalog
            .data
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["typesafe/jev-latest", "typesafe/future-system-one"]
    );
    assert_eq!(
        catalog.data[0].created,
        chrono::DateTime::parse_from_rfc3339("2026-09-17T00:00:00Z")
            .unwrap()
            .timestamp()
    );
    for model in &catalog.data {
        let capabilities = model.capabilities.as_ref().unwrap();
        assert_eq!(
            capabilities.surfaces[&ModelSurface::Decisions],
            Support::Supported
        );
        assert_eq!(
            capabilities
                .operation(Protocol::RouterDecisions, ProtocolOperation::Create)
                .unwrap()
                .mode,
            DeliveryMode::Adapted
        );
        assert_eq!(
            capabilities
                .operation(Protocol::TypesafeSystemOne, ProtocolOperation::Create)
                .unwrap()
                .mode,
            DeliveryMode::Native
        );
    }
    let future = router
        .get_model("typesafe/future-system-one")
        .await
        .unwrap();
    assert_eq!(future.id, "typesafe/future-system-one");
    assert_eq!(
        future.capabilities.unwrap().surfaces[&ModelSurface::Decisions],
        Support::Supported
    );

    let rate_store = Arc::new(MemoryRateLimitPolicyStore::new("test".to_string()));
    let key_hash = KeyHasher::new(b"test-pepper".to_vec()).hash(TEST_API_KEY);
    rate_store
        .create(RateLimitPolicy {
            quota_policy_id: "decisions-concurrency".to_string(),
            id: "decisions-concurrency-source".to_string(),
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
    let quota = Arc::new(FinishRecordingQuota::default());
    let mut state = AppStateBuilder::new(router).build().await;
    state.rate_limit_store = rate_store.clone();
    state.quota_resolver = Arc::new(QuotaPolicyResolver::new(
        "test".to_string(),
        state.budget_store.clone(),
        rate_store,
    ));
    state.quota_backend = quota.clone();

    let response = app(state)
        .oneshot(request(
            "/v1/alpha/decisions",
            decision_body("typesafe/jev-latest"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(&response_body(response).await).unwrap();
    assert_eq!(body["model"], "typesafe/jev-1.13.0");
    assert_eq!(body["provider"], "TypeSafe");
    assert_eq!(body["usage"]["cost"].as_f64(), Some(0.042));
    assert_eq!(quota.finishes.load(Ordering::SeqCst), 1);

    let requests = upstream_state.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["model"], "jev-latest");
    assert_eq!(
        requests[0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["model", "questions", "state"]
    );
    assert_eq!(
        upstream_state.authorization.lock().unwrap().as_slice(),
        [Some("Bearer typesafe-secret".to_string())]
    );
}

#[tokio::test]
async fn alpha_decisions_uses_openrouter_decisions_transport() {
    let upstream_state = UpstreamState::successful("typesafe/jev-1.13");
    let upstream = Router::new()
        .route("/api/alpha/decisions", post(decision_upstream))
        .route("/api/v1/models", get(openrouter_models_upstream))
        .with_state(upstream_state.clone());
    let upstream_base = crate::support::mock::serve(upstream).await;

    let connections = Arc::new(InMemoryProviderConnectionStore::new());
    add_connection(
        &connections,
        "openrouter",
        format!("{upstream_base}/api/v1"),
        "openrouter-secret",
        ExposurePolicy::Unlisted,
    )
    .await;
    let state = AppStateBuilder::new(router_backend(connections))
        .build()
        .await;

    let response = app(state)
        .oneshot(request(
            "/v1/alpha/decisions",
            decision_body("openrouter/typesafe/jev-1.13"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(&response_body(response).await).unwrap();
    assert_eq!(body["model"], "openrouter/typesafe/jev-1.13");
    assert_eq!(body["provider"], "OpenRouter");

    let requests = upstream_state.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["model"], "typesafe/jev-1.13");
    assert_eq!(requests[0]["session_id"], "workflow-1");
    assert_eq!(
        upstream_state.authorization.lock().unwrap().as_slice(),
        [Some("Bearer openrouter-secret".to_string())]
    );
}

#[tokio::test]
async fn system_one_falls_back_from_typesafe_to_openrouter_and_renders_native_shape() {
    let typesafe_state = UpstreamState::failing(StatusCode::SERVICE_UNAVAILABLE);
    let typesafe = Router::new()
        .route("/v1/systemone", post(decision_upstream))
        .route("/v1/models", get(typesafe_models_upstream))
        .with_state(typesafe_state.clone());
    let typesafe_base = crate::support::mock::serve(typesafe).await;
    let openrouter_state = UpstreamState::successful("typesafe/jev-1.13");
    let openrouter = Router::new()
        .route("/api/v1/systemone", post(decision_upstream))
        .route("/api/v1/models", get(openrouter_models_upstream))
        .with_state(openrouter_state.clone());
    let openrouter_base = crate::support::mock::serve(openrouter).await;

    let connections = Arc::new(InMemoryProviderConnectionStore::new());
    add_connection(
        &connections,
        "typesafe",
        format!("{typesafe_base}/v1"),
        "typesafe-secret",
        ExposurePolicy::Unlisted,
    )
    .await;
    add_connection(
        &connections,
        "openrouter",
        format!("{openrouter_base}/api/v1"),
        "openrouter-secret",
        ExposurePolicy::Unlisted,
    )
    .await;
    let state = AppStateBuilder::new(router_backend(connections))
        .fallback_policy(FallbackPolicy::from_map(HashMap::from([(
            "typesafe/jev-1.13.0".to_string(),
            vec!["openrouter/typesafe/jev-1.13".to_string()],
        )])))
        .build()
        .await;

    let response = app(state)
        .oneshot(request("/v1/systemone", system_one_body("jev-1.13.0")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["x-fallback-model"],
        "openrouter/typesafe/jev-1.13"
    );
    let body: serde_json::Value = serde_json::from_slice(&response_body(response).await).unwrap();
    assert_eq!(body["model"], "jev-1.13");
    assert!(body.get("id").is_none());
    assert!(body.get("provider").is_none());
    assert!(body["usage"].get("cost").is_none());
    assert_eq!(
        typesafe_state.requests.lock().unwrap()[0]["model"],
        "jev-1.13.0"
    );
    assert_eq!(
        openrouter_state.requests.lock().unwrap()[0]["model"],
        "typesafe/jev-1.13"
    );
}

#[derive(Clone, Default)]
struct MockDecisionsBackend {
    calls: Arc<Mutex<Vec<DecisionsRequest>>>,
    failure: Arc<Mutex<Option<StatusCode>>>,
}

impl MockDecisionsBackend {
    fn failing(status: StatusCode) -> Self {
        Self {
            failure: Arc::new(Mutex::new(Some(status))),
            ..Default::default()
        }
    }

    fn called_models(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.model.clone())
            .collect()
    }

    fn route(&self, public_model: &str) -> ResolvedRoute {
        let (provider_kind, stripped_model) = public_model.split_once('/').unwrap();
        let capabilities =
            openai_server::capabilities::resolve(provider_kind, stripped_model, None, None)
                .unwrap();
        ResolvedRoute {
            routing: Default::default(),
            backend: Arc::new(self.clone()),
            stripped_model: stripped_model.to_string(),
            capabilities: Arc::new(capabilities),
            resolution: RouteResolution {
                provider_kind: provider_kind.to_string(),
                connection_id: Some(format!("{provider_kind}-test")),
                connection_name: format!("{provider_kind}-test"),
                upstream_model_id: stripped_model.to_string(),
                connection_prefix: None,
                model_route_id: None,
            },
        }
    }

    fn response(&self, model: &str) -> reqwest::Response {
        let status = *self.failure.lock().unwrap();
        let (status, content_type, body) = match status {
            Some(StatusCode::SERVICE_UNAVAILABLE) => (
                503,
                "application/problem+json",
                br#"{"error":{"message":"typesafe overloaded"}}"#.to_vec(),
            ),
            Some(StatusCode::BAD_REQUEST) => (
                400,
                "application/problem+json",
                br#"{"error":{"message":"invalid decision"}}"#.to_vec(),
            ),
            _ => (
                200,
                "application/json",
                serde_json::to_vec(&serde_json::json!({
                    "model": model,
                    "answers": {"team": {"type": "choice", "choice": "payments"}},
                    "usage": {"input_tokens": 12, "output_tokens": 3}
                }))
                .unwrap(),
            ),
        };
        http::Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, content_type)
            .body(body)
            .unwrap()
            .into()
    }
}

#[async_trait]
impl BackendProvider for MockDecisionsBackend {
    async fn send_decisions_request(
        &self,
        request: DecisionsRequest,
    ) -> Result<reqwest::Response, BackendError> {
        let model = request.model.clone();
        self.calls.lock().unwrap().push(request);
        Ok(self.response(&model))
    }

    async fn list_models(&self) -> Result<ListResponse<Model>, BackendError> {
        Ok(ListResponse {
            object: "list".to_string(),
            data: Vec::new(),
            first_id: None,
            last_id: None,
            has_more: false,
        })
    }

    async fn get_model(&self, model_id: &str) -> Result<Model, BackendError> {
        Err(BackendError::api_status(
            404,
            format!("model '{model_id}' not found"),
        ))
    }

    fn name(&self) -> &str {
        "mock-decisions"
    }

    async fn resolve(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        Ok(self.route(model))
    }

    async fn resolve_strict(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        Ok(self.route(model))
    }
}

async fn mock_state(
    backend: Arc<MockDecisionsBackend>,
    permission: ApiKeyPermission,
    fallbacks: FallbackPolicy,
) -> openai_server::AppState {
    let hasher = KeyHasher::new(b"test-pepper".to_vec());
    let key_store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(hasher.clone()));
    key_store
        .insert_raw(ApiKey {
            key_hash: hasher.hash(TEST_API_KEY),
            key_prefix: openai_server::hashing::key_prefix(TEST_API_KEY),
            description: "Decisions test key".to_string(),
            created_at: chrono::Utc::now(),
            permission,
            labels: HashMap::new(),
            usage_pool_id: None,
            daily_token_budget: None,
        })
        .await
        .unwrap();
    AppStateBuilder::new(backend)
        .api_key_store(key_store)
        .fallback_policy(fallbacks)
        .build()
        .await
}

#[tokio::test]
async fn final_retryable_failure_preserves_the_upstream_response() {
    let backend = Arc::new(MockDecisionsBackend::failing(
        StatusCode::SERVICE_UNAVAILABLE,
    ));
    let state = mock_state(
        backend.clone(),
        ApiKeyPermission::default(),
        FallbackPolicy::empty(),
    )
    .await;
    let response = app(state)
        .oneshot(request(
            "/v1/alpha/decisions",
            decision_body("typesafe/jev-1.13.0"),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/problem+json"
    );
    assert_eq!(
        response_body(response).await.as_ref(),
        br#"{"error":{"message":"typesafe overloaded"}}"#
    );
    assert_eq!(backend.called_models(), ["jev-1.13.0"]);
}

#[tokio::test]
async fn client_error_is_terminal_and_does_not_try_a_fallback() {
    let backend = Arc::new(MockDecisionsBackend::failing(StatusCode::BAD_REQUEST));
    let state = mock_state(
        backend.clone(),
        ApiKeyPermission::default(),
        FallbackPolicy::from_map(HashMap::from([(
            "typesafe/jev-1.13.0".to_string(),
            vec!["openrouter/typesafe/jev-1.13".to_string()],
        )])),
    )
    .await;
    let response = app(state)
        .oneshot(request(
            "/v1/alpha/decisions",
            decision_body("typesafe/jev-1.13.0"),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response_body(response).await.as_ref(),
        br#"{"error":{"message":"invalid decision"}}"#
    );
    assert_eq!(backend.called_models(), ["jev-1.13.0"]);
}

#[tokio::test]
async fn permission_and_capability_rejections_happen_before_dispatch() {
    let backend = Arc::new(MockDecisionsBackend::default());
    let restricted = mock_state(
        backend.clone(),
        ApiKeyPermission {
            provider: Some("openai".to_string()),
            ..Default::default()
        },
        FallbackPolicy::empty(),
    )
    .await;
    let response = app(restricted)
        .oneshot(request(
            "/v1/alpha/decisions",
            decision_body("typesafe/jev-1.13.0"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(backend.called_models().is_empty());

    let unrestricted = mock_state(
        backend.clone(),
        ApiKeyPermission::default(),
        FallbackPolicy::empty(),
    )
    .await;
    let response = app(unrestricted)
        .oneshot(request(
            "/v1/alpha/decisions",
            decision_body("openrouter/openai/gpt-4o"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = String::from_utf8(response_body(response).await.to_vec()).unwrap();
    assert!(body.contains("no configured provider supports Router Decisions requests"));
    assert!(backend.called_models().is_empty());
}

#[tokio::test]
async fn typesafe_aliases_dispatch_on_both_ingresses() {
    let backend = Arc::new(MockDecisionsBackend::default());
    let state = mock_state(
        backend.clone(),
        ApiKeyPermission::default(),
        FallbackPolicy::empty(),
    )
    .await;
    let alpha = app(state.clone())
        .oneshot(request(
            "/v1/alpha/decisions",
            decision_body("typesafe/jev-latest"),
        ))
        .await
        .unwrap();
    assert_eq!(alpha.status(), StatusCode::OK);
    let alpha_body: serde_json::Value =
        serde_json::from_slice(&response_body(alpha).await).unwrap();
    let cost = alpha_body["usage"]["cost"].as_f64().unwrap();
    assert!((cost - 0.000000504).abs() < 1e-12);

    let system_one = app(state)
        .oneshot(request("/v1/systemone", system_one_body("jev-preview")))
        .await
        .unwrap();
    assert_eq!(system_one.status(), StatusCode::OK);
    assert_eq!(backend.called_models(), ["jev-latest", "jev-preview"]);
}
