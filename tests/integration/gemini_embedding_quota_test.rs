//! Image embeddings use real auth, quota admission, upstream HTTP and settlement.
use std::sync::{
    atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicUsize, Ordering},
    Arc,
};

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use openai_server::{
    api_keys::{ApiKeyPermission, ApiKeyStore, InMemoryApiKeyStore, PostgresApiKeyStore},
    backend::RouterBackend,
    budget::{self, BudgetPolicyStore},
    capabilities::CapabilityPatch,
    handlers,
    hashing::KeyHasher,
    middleware::auth_middleware,
    model_routes::InMemoryModelRouteStore,
    provider_connections::{
        ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore, ProviderConnectionStore,
    },
    quota::{FixedWindowUsageQuery, MemoryQuota, PostgresQuota, QuotaBackend},
    quota_policy::QuotaPolicyResolver,
    ratelimit::{self, ApiKeyRateLimitInput},
    service::api_keys::{ApiKeyService, CreateKeyInput, UpdateKeyInput},
    storage::ObjectStore,
    usage_tracker::UsageTracker,
    AppState,
};
use serde_json::{json, Value};
use tokio::sync::{Notify, Semaphore};
use tower::ServiceExt;

use super::support::{app::AppStateBuilder, mock::serve};

const MODEL: &str = "gemini/gemini-embedding-2";
const IMAGE: &str = "data:image/png;base64,aW1hZ2U=";

struct Upstream {
    calls: AtomicUsize,
    image_fetches: AtomicUsize,
    tokens: AtomicI32,
    status: AtomicU16,
    hold: AtomicBool,
    started: Notify,
    release: Semaphore,
}

async fn embed(State(upstream): State<Arc<Upstream>>, Json(_body): Json<Value>) -> Response {
    upstream.calls.fetch_add(1, Ordering::SeqCst);
    upstream.started.notify_one();
    if upstream.hold.load(Ordering::SeqCst) {
        upstream.release.acquire().await.unwrap().forget();
    }
    let status = StatusCode::from_u16(upstream.status.load(Ordering::SeqCst)).unwrap();
    if !status.is_success() {
        return (
            status,
            [("retry-after", "2")],
            Json(json!({"error":{"message":"embedding service unavailable"}})),
        )
            .into_response();
    }
    let mut response = json!({"embedding":{"values":[0.5,-0.25]}});
    let tokens = upstream.tokens.load(Ordering::SeqCst);
    if tokens >= 0 {
        response["usageMetadata"] = json!({"promptTokenCount":tokens});
    }
    Json(response).into_response()
}

struct Fixture {
    state: AppState,
    token: String,
    key: String,
    env: String,
    upstream: Arc<Upstream>,
    upstream_url: String,
    service: ApiKeyService,
    budgets: Arc<dyn BudgetPolicyStore>,
    quota: Arc<dyn QuotaBackend>,
    pool: Option<sqlx::PgPool>,
}

fn rates(
    tpm: Option<u64>,
    rpm: Option<u64>,
    concurrency: Option<u64>,
) -> Vec<ApiKeyRateLimitInput> {
    vec![ApiKeyRateLimitInput {
        connection_id: None,
        models: vec![],
        requests_per_minute: rpm,
        request_burst: rpm,
        tokens_per_minute: tpm,
        token_burst: tpm,
        concurrent_requests: concurrency,
        enabled: true,
    }]
}

async fn fixture(
    pool: Option<sqlx::PgPool>,
    budget: u64,
    rates: Vec<ApiKeyRateLimitInput>,
) -> Fixture {
    fixture_with_declaration(pool, budget, rates, None).await
}

async fn fixture_with_declaration(
    pool: Option<sqlx::PgPool>,
    budget: u64,
    rates: Vec<ApiKeyRateLimitInput>,
    declared_capabilities: Option<CapabilityPatch>,
) -> Fixture {
    let env = format!("embedding-quota-{}", uuid::Uuid::new_v4());
    let upstream = Arc::new(Upstream {
        calls: AtomicUsize::new(0),
        image_fetches: AtomicUsize::new(0),
        tokens: AtomicI32::new(258),
        status: AtomicU16::new(200),
        hold: AtomicBool::new(false),
        started: Notify::new(),
        release: Semaphore::new(0),
    });
    let upstream_url = serve(
        Router::new()
            .route("/v1beta/models/:operation", post(embed))
            .route(
                "/image.png",
                get(|State(upstream): State<Arc<Upstream>>| async move {
                    upstream.image_fetches.fetch_add(1, Ordering::SeqCst);
                    ([("content-type", "image/png")], b"image")
                }),
            )
            .with_state(upstream.clone()),
    )
    .await;
    let connections = Arc::new(InMemoryProviderConnectionStore::new());
    connections
        .create(ConnectionInput {
            name: "gemini-test".into(),
            provider_kind: "gemini".into(),
            base_url: Some(format!("{upstream_url}/v1beta/openai")),
            prefix: Some("gemini/".into()),
            exposure: ExposurePolicy::Dynamic,
            routing_enabled: true,
            declared_capabilities,
            api_key: "not-a-real-provider-key".into(),
        })
        .await
        .unwrap();
    let hasher = KeyHasher::new(b"embedding-quota-test".to_vec());
    let keys: Arc<dyn ApiKeyStore> = match &pool {
        Some(pool) => Arc::new(PostgresApiKeyStore::new(pool.clone(), hasher, env.clone())),
        None => Arc::new(InMemoryApiKeyStore::new(hasher)),
    };
    let budgets = budget::from_core_store(pool.clone(), env.clone());
    let rate_store = ratelimit::from_core_store(pool.clone(), env.clone());
    let tracker = match &pool {
        Some(pool) => Some(UsageTracker::new(pool.clone(), env.clone()).await.unwrap()),
        None => None,
    };
    let service = ApiKeyService::new(
        keys.clone(),
        tracker.clone(),
        budgets.clone(),
        rate_store.clone(),
        std::sync::Arc::new(
            openai_server::provider_connections::InMemoryProviderConnectionStore::new(),
        ),
        std::sync::Arc::new(openai_server::model_routes::InMemoryModelRouteStore::new()),
        true,
    );
    let created = service
        .create(CreateKeyInput {
            description: Some("Gemini embedding quota test".into()),
            daily_token_budget: Some(budget),
            rate_limits: rates,
            ..Default::default()
        })
        .await
        .unwrap();
    let quota: Arc<dyn QuotaBackend> = match &pool {
        Some(pool) => Arc::new(PostgresQuota::new(pool.clone())),
        None => Arc::new(MemoryQuota::new()),
    };
    let mut state = AppStateBuilder::new(Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        connections,
        Arc::new(InMemoryModelRouteStore::new()),
    )))
    .environment_id(env.clone())
    .api_key_store(keys)
    .budget_store(budgets.clone())
    .rate_limit_store(rate_store.clone())
    .quota_resolver(Arc::new(QuotaPolicyResolver::new(
        env.clone(),
        budgets.clone(),
        rate_store,
    )))
    .quota_backend(quota.clone())
    .build()
    .await;
    state.usage_tracker = tracker;
    Fixture {
        state,
        token: created.raw_key,
        key: created.api_key.key_hash.clone(),
        env,
        upstream,
        upstream_url,
        service,
        budgets,
        quota,
        pool,
    }
}

async fn send(state: AppState, token: &str, body: Value) -> (StatusCode, HeaderMap, Value) {
    let app = Router::new()
        .route(
            "/v1/embeddings",
            post(handlers::embeddings::create_embedding),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);
    let response = app
        .oneshot(
            Request::post("/v1/embeddings")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, serde_json::from_slice(&bytes).unwrap())
}

fn image_request(url: &str) -> Value {
    json!({"model":MODEL,"input":[{"type":"image_url","image_url":{"url":url}}]})
}

impl Fixture {
    async fn send(&self, body: Value) -> (StatusCode, HeaderMap, Value) {
        send(self.state.clone(), &self.token, body).await
    }

    async fn used(&self) -> u64 {
        let policy = self
            .budgets
            .matching(&self.key, None)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let window_start = Utc::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        self.quota
            .fixed_window_usage(&[FixedWindowUsageQuery {
                policy_id: policy.quota_policy_id.clone(),
                window_start,
            }])
            .await
            .unwrap()
            .unwrap()[&policy.quota_policy_id]
    }

    async fn assert_ledger(&self, count: i64, tokens: i64) {
        if let Some(pool) = &self.pool {
            let row: (i64, i64) = sqlx::query_as("SELECT COUNT(*)::BIGINT, COALESCE(SUM(total_tokens),0)::BIGINT FROM chalk_router.usage_events WHERE environment_id=$1")
                .bind(&self.env).fetch_one(pool).await.unwrap();
            assert_eq!(row, (count, tokens));
        }
    }
}

#[tokio::test]
async fn declared_metadata_cannot_lower_embedding_reservations_or_raise_image_limits() {
    for embedding_input in [
        Value::Null,
        json!({"max_images_per_document": 1000, "image_mime_types": ["image/gif"]}),
    ] {
        let declared: CapabilityPatch = serde_json::from_value(json!({
            "schema_version": 1,
            "model_defaults": {"limits": {"max_input_tokens": 1, "embedding_input": embedding_input}}
        })).unwrap();
        let f = fixture_with_declaration(None, 8191, vec![], Some(declared.clone())).await;
        let (status, _, body) = f.send(image_request(IMAGE)).await;
        assert_eq!(status, 429, "{body}");
        assert_eq!(f.upstream.calls.load(Ordering::SeqCst), 0);
        assert_eq!(f.used().await, 0);

        let f = fixture_with_declaration(None, 100_000, vec![], Some(declared)).await;
        let images: Vec<_> = (0..7)
            .map(|_| json!({"type":"image_url", "image_url":{"url":IMAGE}}))
            .collect();
        let (status, _, body) = f.send(json!({"model": MODEL, "input": images})).await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(f.upstream.calls.load(Ordering::SeqCst), 0);
        assert_eq!(f.used().await, 0);
    }
}

async fn admission_and_settlement(pool: Option<sqlx::PgPool>) {
    // Both account-level budget and global TPM require the full document ceiling.
    for (budget, tpm) in [(8191, 100_000), (100_000, 8191)] {
        let mut f = fixture(pool.clone(), budget, rates(Some(tpm), None, None)).await;
        // A configured object store makes a pre-admission tracing fetch observable.
        f.state.object_store = Some(Arc::new(
            ObjectStore::from_uri("gs://embedding-test-unused").unwrap(),
        ));
        let (status, _, body) = f
            .send(image_request(&format!("{}/image.png", f.upstream_url)))
            .await;
        assert_eq!(status, 429, "{body}");
        assert_eq!(f.upstream.calls.load(Ordering::SeqCst), 0);
        assert_eq!(f.upstream.image_fetches.load(Ordering::SeqCst), 0);
        assert_eq!(f.used().await, 0);
        f.assert_ledger(0, 0).await;
    }

    let f = fixture(pool.clone(), 9000, rates(Some(9000), None, None)).await;
    f.upstream.hold.store(true, Ordering::SeqCst);
    let state = f.state.clone();
    let token = f.token.clone();
    let task = tokio::spawn(async move { send(state, &token, image_request(IMAGE)).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        f.upstream.started.notified(),
    )
    .await
    .unwrap();
    assert_eq!(
        f.used().await,
        8192,
        "reserve the whole document while upstream is in flight"
    );
    let (status, _, _) = f.send(image_request(IMAGE)).await;
    assert_eq!(
        status, 429,
        "a concurrent request cannot spend the same reservation"
    );
    assert_eq!(f.upstream.calls.load(Ordering::SeqCst), 1);
    f.upstream.release.add_permits(1);
    assert_eq!(task.await.unwrap().0, 200);
    assert_eq!(f.used().await, 258);
    f.assert_ledger(1, 258).await;
    f.upstream.hold.store(false, Ordering::SeqCst);
    f.upstream.tokens.store(265, Ordering::SeqCst);
    let (status,_,body) = f.send(json!({"model":"gemini/models/gemini-embedding-2-preview","dimensions":768,"encoding_format":"base64", "input":[
        {"type":"text","text":"caption"},{"type":"image_url","image_url":{"url":IMAGE}}
    ]})).await;
    assert_eq!(status, 200, "unused tokens must be released: {body}");
    assert_eq!(f.used().await, 523);
    f.assert_ledger(2, 523).await;

    // A missing usage object remains an accounting failure; the upper bound is
    // never recorded as an invented estimate. Unknown usage retains reservation.
    f.upstream.tokens.store(-1, Ordering::SeqCst);
    let (status, _, body) = f.send(image_request(IMAGE)).await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(f.used().await, 523 + 8192);
    f.assert_ledger(2, 523).await;
}

#[tokio::test]
async fn memory_image_admission_and_actual_settlement() {
    admission_and_settlement(None).await;
}

#[tokio::test]
async fn postgres_image_admission_and_actual_settlement() {
    let Some(url) = super::support::db::test_database_url("Gemini embedding quota") else {
        return;
    };
    let _lock = super::support::db::connect_locked(&url).await;
    openai_server::migrate::run(&url).await.unwrap();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .unwrap();
    admission_and_settlement(Some(pool)).await;
}

#[tokio::test]
async fn rpm_is_charged_once_and_concurrency_is_released() {
    let f = fixture(None, 100_000, rates(None, Some(2), Some(1))).await;
    for _ in 0..2 {
        let (status, _, body) = f.send(image_request(IMAGE)).await;
        assert_eq!(status, 200, "{body}");
    }
    assert_eq!(f.send(image_request(IMAGE)).await.0, 429);
    assert_eq!(f.upstream.calls.load(Ordering::SeqCst), 2);

    let f = fixture(None, 100_000, rates(None, None, Some(1))).await;
    f.upstream.hold.store(true, Ordering::SeqCst);
    let state = f.state.clone();
    let token = f.token.clone();
    let task = tokio::spawn(async move { send(state, &token, image_request(IMAGE)).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        f.upstream.started.notified(),
    )
    .await
    .unwrap();
    assert_eq!(f.send(image_request(IMAGE)).await.0, 429);
    f.upstream.release.add_permits(1);
    assert_eq!(task.await.unwrap().0, 200);
    f.upstream.hold.store(false, Ordering::SeqCst);
    f.upstream.tokens.store(-1, Ordering::SeqCst);
    assert_eq!(f.send(image_request(IMAGE)).await.0, 503);
    f.upstream.tokens.store(258, Ordering::SeqCst);
    assert_eq!(
        f.send(image_request(IMAGE)).await.0,
        200,
        "accounting error releases concurrency"
    );
}

#[tokio::test]
async fn image_limit_and_permission_denials_precede_fetch() {
    let mut f = fixture(None, 100_000, vec![]).await;
    f.state.object_store = Some(Arc::new(
        ObjectStore::from_uri("gs://embedding-test-unused").unwrap(),
    ));
    let url = format!("{}/image.png", f.upstream_url);
    let image = json!({"type":"image_url","image_url":{"url":url}});
    assert_eq!(
        f.send(json!({"model":MODEL,"input":vec![image;7]})).await.0,
        400
    );
    f.service
        .update(
            &f.key,
            UpdateKeyInput {
                permission: Some(
                    ApiKeyPermission {
                        route_ids: Some(Vec::new()),
                        ..Default::default()
                    }
                    .into(),
                ),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(f.send(image_request(&url)).await.0, 403);
    f.service
        .update(
            &f.key,
            UpdateKeyInput {
                permission: Some(ApiKeyPermission::default().into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut request = image_request(&url);
    request["model"] = json!("gemini/gemini-embedding-001");
    assert_eq!(f.send(request).await.0, 400);
    assert_eq!(f.used().await, 0);
    assert_eq!(f.upstream.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.upstream.image_fetches.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn text_requests_keep_their_small_reservation_and_usage_fallback() {
    let f = fixture(None, 200, rates(Some(200), None, None)).await;
    f.upstream.tokens.store(-1, Ordering::SeqCst);
    let (status, _, body) = f.send(json!({"model":MODEL,"input":"short text"})).await;
    assert_eq!(status, 200, "{body}");
    assert!(f.used().await > 0 && f.used().await < 200);
}

#[tokio::test]
async fn upstream_failure_keeps_the_bound_and_releases_concurrency_without_usage() {
    let f = fixture(None, 100_000, rates(None, None, Some(1))).await;
    f.upstream.status.store(503, Ordering::SeqCst);
    let (status, headers, body) = f.send(image_request(IMAGE)).await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(headers["retry-after"], "2");
    assert_eq!(f.used().await, 8192);
    assert_eq!(f.upstream.calls.load(Ordering::SeqCst), 1);
    f.upstream.status.store(200, Ordering::SeqCst);
    f.upstream.tokens.store(1548, Ordering::SeqCst);
    let image = json!({"type":"image_url","image_url":{"url":IMAGE}});
    let (status, _, body) = f.send(json!({"model":MODEL,"input":vec![image;6]})).await;
    assert_eq!(status, 200, "six images are still one document: {body}");
    assert_eq!(f.used().await, 8192 + 1548);
}
