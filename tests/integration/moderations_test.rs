//! Moderation goes through real route resolution, auth, quota and upstream HTTP.
use std::{collections::HashMap, sync::Arc};

use super::support::{app::AppStateBuilder, mock::serve};
use axum::{
    extract::State,
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use openai_server::{
    api_keys::{ApiKeyPermission, ApiKeyStore, InMemoryApiKeyStore},
    backend::RouterBackend,
    budget::MemoryBudgetPolicyStore,
    handlers,
    hashing::KeyHasher,
    middleware::auth_middleware,
    model_routes::InMemoryModelRouteStore,
    provider_connections::{
        ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore, ProviderConnectionStore,
    },
    quota::MemoryQuota,
    quota_policy::QuotaPolicyResolver,
    ratelimit::{MemoryRateLimitPolicyStore, RateLimitPolicy, RateLimitPolicyStore},
    AppState,
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

const MODEL: &str = "moderation/omni-moderation-latest";
const ERROR: &str = r#"{ "error": {"message":"provider moderation failure", "type":"invalid_request_error", "code":"moderation_error"}, "request_id":"upstream-123" }"#;

fn verdict() -> Value {
    json!({
        "id":"modr-real", "model":"omni-moderation-latest",
        "results":[{
            "flagged":true,
            "categories":{"sexual":false,"hate":false,"harassment":false,"violence":true,"self-harm/intent":true,"illicit/violent":true},
            "category_scores":{"sexual":0.0,"hate":0.0,"harassment":0.0,"violence":0.97,"self-harm/intent":0.9,"illicit/violent":0.8},
            "category_applied_input_types":{"violence":["text"]}
        }]
    })
}

#[derive(Default)]
struct Upstream {
    requests: Mutex<Vec<Value>>,
}
async fn moderate(State(upstream): State<Arc<Upstream>>, Json(body): Json<Value>) -> Response {
    let error = body["input"].as_str().and_then(|v| v.parse::<u16>().ok());
    upstream.requests.lock().await.push(body);
    if let Some(status) = error {
        return (
            StatusCode::from_u16(status).unwrap(),
            [("content-type", "application/json"), ("retry-after", "2")],
            ERROR,
        )
            .into_response();
    }
    Json(verdict()).into_response()
}

struct Fixture {
    state: AppState,
    raw_key: String,
    key_hash: String,
    connection_id: String,
    upstream: Arc<Upstream>,
}

async fn fixture(permission: ApiKeyPermission) -> Fixture {
    let upstream = Arc::new(Upstream::default());
    let base = serve(
        Router::new()
            .route("/moderations", post(moderate))
            .with_state(upstream.clone()),
    )
    .await;
    let connections = Arc::new(InMemoryProviderConnectionStore::new());
    let connection = connections
        .create(ConnectionInput {
            name: "moderation".into(),
            provider_kind: "openai".into(),
            base_url: Some(base),
            prefix: Some("moderation/".into()),
            exposure: ExposurePolicy::Unlisted,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: "test-upstream-key".into(),
        })
        .await
        .unwrap();
    let routes = Arc::new(InMemoryModelRouteStore::new());
    let backend = Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        connections.clone(),
        routes.clone(),
    ));
    let keys = Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
        b"test-pepper".to_vec(),
    )));
    let key = keys
        .create(
            "moderation-client".into(),
            permission,
            HashMap::new(),
            Some("moderation-pool".into()),
        )
        .await
        .unwrap();
    let state = AppStateBuilder::new(backend)
        .api_key_store(keys)
        .provider_connection_store(connections)
        .model_route_store(routes)
        .build()
        .await;
    Fixture {
        state,
        raw_key: key.raw_key,
        key_hash: key.api_key.key_hash.clone(),
        connection_id: connection.id.clone(),
        upstream,
    }
}

async fn boot(state: AppState) -> String {
    serve(
        Router::new()
            .route(
                "/v1/moderations",
                post(handlers::moderations::create_moderation),
            )
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ))
            .with_state(state),
    )
    .await
}
async fn request(base: &str, key: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/moderations"))
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn forwards_flagged_verdict_and_all_categories_with_stripped_model() {
    let f = fixture(ApiKeyPermission::default()).await;
    let base = boot(f.state).await;
    let response = request(
        &base,
        &f.raw_key,
        json!({"model":MODEL,"input":["first","second"]}),
    )
    .await;
    assert_eq!(response.status(), 200);
    let actual: Value = response.json().await.unwrap();
    assert_eq!(actual["results"][0]["flagged"], true);
    assert_eq!(
        actual["results"][0]["categories"],
        verdict()["results"][0]["categories"]
    );
    assert_eq!(
        actual["results"][0]["category_applied_input_types"],
        verdict()["results"][0]["category_applied_input_types"]
    );
    assert!(
        (actual["results"][0]["category_scores"]["illicit/violent"]
            .as_f64()
            .unwrap()
            - 0.8)
            .abs()
            < 1e-6
    );
    let requests = f.upstream.requests.lock().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0],
        json!({"model":"omni-moderation-latest","input":["first","second"]})
    );
}

#[tokio::test]
async fn omitted_model_resolves_default_and_checks_permission() {
    for allowed in [true, false] {
        let f = fixture(ApiKeyPermission {
            models: Some(vec![if allowed {
                "omni-moderation-latest"
            } else {
                "gpt-5"
            }
            .into()]),
            ..Default::default()
        })
        .await;
        let base = boot(f.state).await;
        let response = request(&base, &f.raw_key, json!({"input":"text"})).await;
        assert_eq!(response.status(), if allowed { 200 } else { 403 });
        assert_eq!(f.upstream.requests.lock().await.len(), usize::from(allowed));
    }
}

#[tokio::test]
async fn permission_denial_precedes_dispatch_for_provider_connection_and_model() {
    for permission in [
        ApiKeyPermission {
            provider: Some("anthropic".into()),
            ..Default::default()
        },
        ApiKeyPermission {
            connection_id: Some("another-connection".into()),
            ..Default::default()
        },
        ApiKeyPermission {
            models: Some(vec!["moderation/gpt-5".into()]),
            ..Default::default()
        },
    ] {
        let f = fixture(permission).await;
        let base = boot(f.state).await;
        assert_eq!(
            request(&base, &f.raw_key, json!({"model":MODEL,"input":"text"}))
                .await
                .status(),
            403
        );
        assert!(f.upstream.requests.lock().await.is_empty());
    }
}

#[tokio::test]
async fn unsupported_moderation_capability_precedes_upstream_dispatch() {
    let f = fixture(ApiKeyPermission::default()).await;
    f.state.provider_connection_store.update(&f.connection_id, openai_server::provider_connections::ConnectionUpdate {
        declared_capabilities: Some(Some(serde_json::from_value(json!({
            "schema_version":1,
            "endpoint":{"interfaces":{"openai_moderations":{"operations":{"create":{"support":"unsupported"}}}}}
        })).unwrap())),
        ..Default::default()
    }).await.unwrap();
    let base = boot(f.state).await;
    let response = request(&base, &f.raw_key, json!({"model":MODEL,"input":"text"})).await;
    assert_eq!(response.status(), 400);
    assert!(f.upstream.requests.lock().await.is_empty());
}

async fn with_rate(mut f: Fixture, scope: &str, model_scoped: bool, meter: &str) -> Fixture {
    let budgets = Arc::new(MemoryBudgetPolicyStore::new("test".into()));
    let rates = Arc::new(MemoryRateLimitPolicyStore::new("test".into()));
    rates
        .create(RateLimitPolicy {
            quota_policy_id: "moderation-policy".into(),
            id: "moderation-rate".into(),
            limit_type: meter.into(),
            limit_value: 1,
            burst_value: Some(1),
            connection_id: model_scoped.then(|| f.connection_id.clone()),
            models: if model_scoped {
                vec![MODEL.into()]
            } else {
                vec![]
            },
            scope: scope.into(),
            scope_id: if scope == "usage_pool" {
                "moderation-pool".into()
            } else {
                f.key_hash.clone()
            },
            enabled: true,
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    f.state.budget_store = budgets.clone();
    f.state.rate_limit_store = rates.clone();
    f.state.quota_resolver = Arc::new(QuotaPolicyResolver::new("test".into(), budgets, rates));
    f.state.quota_backend = Arc::new(MemoryQuota::new());
    f
}

#[tokio::test]
async fn request_and_route_scoped_rate_limits_prevent_second_dispatch() {
    for (scope, model_scoped) in [("api_key", false), ("usage_pool", false), ("api_key", true)] {
        let f = with_rate(
            fixture(ApiKeyPermission::default()).await,
            scope,
            model_scoped,
            "requests_per_minute",
        )
        .await;
        let base = boot(f.state).await;
        let body = json!({"model":MODEL,"input":"text"});
        assert_eq!(request(&base, &f.raw_key, body.clone()).await.status(), 200);
        let response = request(&base, &f.raw_key, body).await;
        assert_eq!(response.status(), 429);
        assert!(response.headers().contains_key("retry-after"));
        assert_eq!(f.upstream.requests.lock().await.len(), 1);
    }
}

#[tokio::test]
async fn sequential_calls_release_concurrency_and_omit_token_policies() {
    for meter in ["concurrent_requests", "tokens_per_minute"] {
        let f = with_rate(
            fixture(ApiKeyPermission::default()).await,
            "api_key",
            true,
            meter,
        )
        .await;
        let base = boot(f.state).await;
        for _ in 0..2 {
            assert_eq!(
                request(
                    &base,
                    &f.raw_key,
                    json!({"model":MODEL,"input":"long text"})
                )
                .await
                .status(),
                200
            );
        }
        assert_eq!(f.upstream.requests.lock().await.len(), 2);
    }
}

#[tokio::test]
async fn upstream_errors_keep_status_body_and_retry_headers_without_retrying() {
    for status in [400, 429, 503] {
        let f = with_rate(
            fixture(ApiKeyPermission::default()).await,
            "api_key",
            true,
            "concurrent_requests",
        )
        .await;
        let base = boot(f.state).await;
        let response = request(
            &base,
            &f.raw_key,
            json!({"model":MODEL,"input":status.to_string()}),
        )
        .await;
        assert_eq!(response.status().as_u16(), status);
        assert_eq!(response.headers()["retry-after"], "2");
        assert_eq!(response.text().await.unwrap(), ERROR);
        assert_eq!(f.upstream.requests.lock().await.len(), 1);
        assert_eq!(
            request(&base, &f.raw_key, json!({"model":MODEL,"input":"text"}))
                .await
                .status(),
            200
        );
    }
}

#[tokio::test]
async fn postgres_records_one_request_with_resolved_attribution_and_zero_tokens() {
    let Some(url) = super::support::db::test_database_url("moderation usage") else {
        return;
    };
    let mut lock = super::support::db::connect_locked(&url).await;
    openai_server::migrate::run(&url).await.unwrap();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let env = format!("moderation-{}", uuid::Uuid::new_v4());
    let tracker = openai_server::usage_tracker::UsageTracker::new(pool.clone(), env.clone())
        .await
        .unwrap();
    let mut f = fixture(ApiKeyPermission::default()).await;
    f.state.usage_tracker = Some(tracker.clone());
    let base = boot(f.state).await;
    assert_eq!(
        request(&base, &f.raw_key, json!({"model":MODEL,"input":"text"}))
            .await
            .status(),
        200
    );
    assert_eq!(
        request(&base, &f.raw_key, json!({"model":MODEL,"input":"503"}))
            .await
            .status(),
        503
    );
    let snapshots = tracker
        .snapshots(std::slice::from_ref(&f.key_hash))
        .await
        .unwrap();
    assert_eq!(snapshots[&f.key_hash].request_count, 1);
    assert_eq!(snapshots[&f.key_hash].total_tokens, 0);
    let row:(String,String,String,i64)=sqlx::query_as("SELECT connection_id,model,usage_pool_id,total_tokens FROM chalk_router.usage_events WHERE environment_id=$1")
        .bind(&env).fetch_one(&pool).await.unwrap();
    assert_eq!(
        row,
        (
            f.connection_id,
            "omni-moderation-latest".into(),
            "moderation-pool".into(),
            0
        )
    );
    sqlx::query("DELETE FROM chalk_router.usage_events WHERE environment_id=$1")
        .bind(env)
        .execute(&mut lock)
        .await
        .unwrap();
}
