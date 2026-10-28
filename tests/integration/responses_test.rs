//! End-to-end contract tests for the Responses API frontend.
//!
//! These tests exercise the real handler and authentication middleware. The
//! mock supports native Responses and translated Chat dispatch so the
//! assertions cover both proxy and compatibility behavior.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::{body::Body, middleware, routing::post, Router};
use http::{header, Request, StatusCode};
use tower::ServiceExt;

use openai_server::{
    api_keys::{ApiKeyPermission, ApiKeyStore, InMemoryApiKeyStore},
    backend::{
        provider::{BackendJsonResponse, BackendSseResponse, ResponsesRequestHeaders},
        BackendError, BackendProvider, ResolvedRoute, RouteResolution, SseByteStream,
    },
    budget::{BudgetPolicy, BudgetPolicyStore, MemoryBudgetPolicyStore, PostgresBudgetPolicyStore},
    fallback::FallbackPolicy,
    hashing::KeyHasher,
    middleware::auth_middleware,
    quota::{
        MemoryQuota, PostgresQuota, QuotaAction, QuotaBackend, QuotaBackendError, QuotaDecision,
        QuotaGrant, QuotaOutcome,
    },
    ratelimit::{
        MemoryRateLimitPolicyStore, PostgresRateLimitPolicyStore, RateLimitPolicy,
        RateLimitPolicyStore,
    },
    responses::ResponsesRequest,
    types::{
        ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatMessage,
        CreateEmbeddingRequest, CreateEmbeddingResponse, CreateImageRequest,
        CreateModerationRequest, CreateModerationResponse, CreateSpeechRequest, ImageResponse,
        ListResponse, Model, TranscriptionResponse, Usage,
    },
    AppState,
};

use crate::support::app::AppStateBuilder;

const TEST_KEY: &str = "sk-responses-integration-test";

fn accounting_app(state: AppState) -> Router {
    Router::new()
        .route(
            "/v1/responses",
            post(openai_server::handlers::responses::create_response),
        )
        .route(
            "/v1/responses/:response_id",
            axum::routing::get(openai_server::handlers::responses::retrieve_response),
        )
        .route(
            "/v1/responses/:response_id/cancel",
            post(openai_server::handlers::responses::cancel_response),
        )
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, auth_middleware))
}

#[tokio::test]
async fn postgres_background_accounting_conformance() {
    let Some(url) =
        crate::support::db::test_database_url("postgres_background_accounting_conformance")
    else {
        return;
    };
    let _lock = crate::support::db::connect_locked(&url).await;
    openai_server::migrate::run(&url).await.unwrap();
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    for durable in [false, true] {
        let mut setup = setup(ApiKeyPermission::default(), FallbackPolicy::default()).await;
        let environment_id = format!("background-conformance-{}", uuid::Uuid::new_v4());
        setup.state.environment_id = environment_id.clone();
        setup.state.usage_tracker = Some(
            openai_server::usage_tracker::UsageTracker::new(pool.clone(), environment_id.clone())
                .await
                .unwrap(),
        );
        setup.backend = Arc::new(MockBackend {
            background_queued: true,
            ..MockBackend::default()
        });
        setup.state.backend = setup.backend.clone();
        setup.state.background_responses = Arc::new(
            openai_server::background_responses::BackgroundResponses::new(
                durable.then(|| pool.clone()),
                environment_id.clone(),
                openai_server::background_responses::PollConfig::new(1, 10, 2),
            ),
        );
        let app = accounting_app(setup.state.clone());
        let response=app.clone().oneshot(request("/v1/responses",serde_json::json!({"model":"openai/gpt-5-codex","input":"private-background-prompt","background":true}))).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert_eq!(body["status"], "queued");
        let service = setup.state.background_response_service();
        let page = service.list(None, None, None).await.unwrap();
        assert_eq!(page.jobs.len(), 1);
        assert_eq!(page.jobs[0].phase, "active");
        assert!(!format!("{:?}", page.jobs).contains("private-background-prompt"));
        assert!(!format!("{:?}", page.jobs).contains(TEST_KEY));
        if durable {
            let payload:String=sqlx::query_scalar("SELECT payload::text FROM chalk_router.background_response_jobs WHERE environment_id=$1")
                .bind(&environment_id).fetch_one(&pool).await.unwrap();
            assert!(!payload.contains("private-background-prompt"));
            setup.state.background_responses = Arc::new(
                openai_server::background_responses::BackgroundResponses::new(
                    Some(pool.clone()),
                    environment_id.clone(),
                    openai_server::background_responses::PollConfig::new(1, 10, 2),
                ),
            );
        }
        let runtime = setup.state.background_responses.clone();
        // One instance serializes its own reconciles, so the durable claim race
        // needs a second instance, as another replica would be. An in-memory
        // runtime has no replicas: its two calls run one after the other.
        let replica = if durable {
            Arc::new(
                openai_server::background_responses::BackgroundResponses::new(
                    Some(pool.clone()),
                    environment_id.clone(),
                    openai_server::background_responses::PollConfig::new(1, 10, 2),
                ),
            )
        } else {
            runtime.clone()
        };
        let (first, second) = tokio::join!(
            runtime.reconcile(&setup.state),
            replica.reconcile(&setup.state)
        );
        first.unwrap();
        second.unwrap();
        let replay = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/responses/{}", body["id"].as_str().unwrap()))
                    .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::OK);
        let counts:(i64,i64,i64,i64)=sqlx::query_as("SELECT count(*),sum(total_tokens)::bigint,sum(cached_tokens)::bigint,sum(reasoning_tokens)::bigint FROM chalk_router.usage_events WHERE environment_id=$1")
            .bind(&environment_id).fetch_one(&pool).await.unwrap();
        assert_eq!(counts, (1, 5, 1, 1));
        let creator: String = sqlx::query_scalar(
            "SELECT api_key_hash FROM chalk_router.usage_events WHERE environment_id=$1",
        )
        .bind(&environment_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            creator,
            KeyHasher::new(b"responses-test-pepper".to_vec()).hash(TEST_KEY)
        );
    }
}

#[derive(Clone)]
struct MockBackend {
    background_queued: bool,
    submission_ambiguous: bool,
    submission_pending: bool,
    submission_delay: Option<std::time::Duration>,
    retrieve_gate: Option<Arc<tokio::sync::Semaphore>>,
    chat_calls: Arc<Mutex<Vec<ChatCompletionRequest>>>,
    response_calls: Arc<Mutex<Vec<serde_json::Value>>>,
    stream_calls: Arc<Mutex<Vec<serde_json::Value>>>,
    compact_calls: Arc<Mutex<Vec<serde_json::Value>>>,
    retrieve_calls: Arc<Mutex<Vec<String>>>,
    retrieve_stream_calls: Arc<Mutex<Vec<(String, Option<u64>)>>>,
    cancel_calls: Arc<Mutex<Vec<String>>>,
    delete_calls: Arc<Mutex<Vec<String>>>,
    resolve_calls: Arc<AtomicUsize>,
}

#[tokio::test]
async fn postgres_background_cluster_restart_recovers_idle_tenants_while_another_provider_stalls() {
    use openai_server::{
        background_responses::{BackgroundResponses, PollConfig},
        cluster_lifecycle::ClusterWorkers,
        usage_tracker::UsageTracker,
    };
    let Some(url) = crate::support::db::test_database_url("cluster idle background restart") else {
        return;
    };
    let _lock = crate::support::db::connect_locked(&url).await;
    openai_server::migrate::run(&url).await.unwrap();
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let guard = Arc::new(tokio::sync::Semaphore::new(1));
    let mut states = Vec::new();
    for team in ["a", "b"] {
        let environment = format!("idle-{team}-{}", uuid::Uuid::new_v4());
        let mut setup = setup(ApiKeyPermission::default(), FallbackPolicy::default()).await;
        setup.state.environment_id = environment.clone();
        setup.state.usage_tracker = Some(
            UsageTracker::new(pool.clone(), environment.clone())
                .await
                .unwrap(),
        );
        setup.state.backend = Arc::new(MockBackend {
            background_queued: true,
            retrieve_gate: (team == "a").then(|| guard.clone()),
            ..Default::default()
        });
        setup.state.background_responses = Arc::new(BackgroundResponses::new(
            Some(pool.clone()),
            environment.clone(),
            PollConfig::new(1, 10, 2),
        ));
        let response = accounting_app(setup.state.clone()).oneshot(request("/v1/responses", serde_json::json!({"model":"openai/gpt-5-codex","input":"private queued task","background":true}))).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert_eq!(body["status"], "queued");
        // Reconstruct polling and accounting without replaying an HTTP request.
        // Both upstreams deliberately return the same response ID and usage.
        setup.state.usage_tracker = Some(
            UsageTracker::new(pool.clone(), environment.clone())
                .await
                .unwrap(),
        );
        setup.state.background_responses = Arc::new(BackgroundResponses::new(
            Some(pool.clone()),
            environment,
            PollConfig::new(1, 10, 2),
        ));
        states.push(setup.state);
    }
    let environments: Vec<_> = states
        .iter()
        .map(|state| state.environment_id.clone())
        .collect();
    // Tenant A's upstream cannot answer; B must still recover and settle.
    let held = guard.acquire().await.unwrap();
    let workers = ClusterWorkers::start(
        openai_server::tenancy::EnvironmentRegistry::new(states.clone()).unwrap(),
        Default::default(),
        openai_server::config::QuotaBackendKind::Disabled,
        None,
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let phases: Vec<String> = sqlx::query_scalar("SELECT phase FROM chalk_router.background_response_jobs WHERE environment_id=ANY($1) ORDER BY environment_id").bind(&environments).fetch_all(&pool).await.unwrap();
            if phases == ["active", "settled"] { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }).await.expect("healthy tenant must settle while the other provider is stalled");
    drop(held);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let settled: i64 = sqlx::query_scalar("SELECT count(*) FROM chalk_router.background_response_jobs WHERE environment_id=ANY($1) AND phase='settled'").bind(&environments).fetch_one(&pool).await.unwrap();
            if settled == 2 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }).await.expect("both idle environments must reconcile after startup");
    workers.shutdown().await;
    for state in states {
        state.background_responses.reconcile(&state).await.unwrap();
        let counts: (i64, i64, i64, i64) = sqlx::query_as("SELECT count(*),sum(total_tokens)::bigint,sum(cached_tokens)::bigint,sum(reasoning_tokens)::bigint FROM chalk_router.usage_events WHERE environment_id=$1").bind(&state.environment_id).fetch_one(&pool).await.unwrap();
        assert_eq!(counts, (1, 5, 1, 1));
    }
    pool.close().await;
}

#[tokio::test]
async fn postgres_background_quota_and_usage_settle_atomically_after_restart() {
    let Some(url) = crate::support::db::test_database_url(
        "postgres_background_quota_and_usage_settle_atomically_after_restart",
    ) else {
        return;
    };
    let _lock = crate::support::db::connect_locked(&url).await;
    openai_server::migrate::run(&url).await.unwrap();
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let environment_id = format!("background-atomic-{}", uuid::Uuid::new_v4());
    let mut setup = setup_with_quota_backend(
        ApiKeyPermission::default(),
        FallbackPolicy::default(),
        None,
        None,
        Arc::new(MockBackend {
            background_queued: true,
            ..MockBackend::default()
        }),
    )
    .await;
    let key_hash = KeyHasher::new(b"responses-test-pepper".to_vec()).hash(TEST_KEY);
    let budget_store = Arc::new(PostgresBudgetPolicyStore::new(
        pool.clone(),
        environment_id.clone(),
    ));
    let budget = budget_store
        .create(BudgetPolicy {
            quota_policy_id: uuid::Uuid::new_v4().to_string(),
            id: uuid::Uuid::new_v4().to_string(),
            environment_id: environment_id.clone(),
            scope: "api_key".into(),
            scope_id: key_hash.clone(),
            period: "daily".into(),
            window_type: "calendar".into(),
            anchor: None,
            timezone: "UTC".into(),
            window_size: sqlx::postgres::types::PgInterval {
                months: 0,
                days: 1,
                microseconds: 0,
            },
            limit_kind: "tokens".into(),
            limit_amount: 1000,
            enabled: true,
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    let rate_store = Arc::new(PostgresRateLimitPolicyStore::new(
        pool.clone(),
        environment_id.clone(),
    ));
    let concurrency = rate_store
        .create(RateLimitPolicy {
            quota_policy_id: String::new(),
            id: uuid::Uuid::new_v4().to_string(),
            limit_type: "concurrent_requests".into(),
            limit_value: 1,
            burst_value: None,
            connection_id: None,
            models: Vec::new(),
            scope: "api_key".into(),
            scope_id: key_hash,
            enabled: true,
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    setup.state.environment_id = environment_id.clone();
    setup.state.quota_resolver = Arc::new(openai_server::quota_policy::QuotaPolicyResolver::new(
        environment_id.clone(),
        budget_store.clone(),
        rate_store.clone(),
    ));
    setup.state.budget_store = budget_store;
    setup.state.rate_limit_store = rate_store;
    setup.state.quota_backend = Arc::new(PostgresQuota::new(pool.clone()));
    setup.state.usage_tracker = Some(
        openai_server::usage_tracker::UsageTracker::new(pool.clone(), environment_id.clone())
            .await
            .unwrap(),
    );
    let runtime = || {
        Arc::new(
            openai_server::background_responses::BackgroundResponses::new(
                Some(pool.clone()),
                environment_id.clone(),
                openai_server::background_responses::PollConfig::new(1, 10, 2),
            ),
        )
    };
    setup.state.background_responses = runtime();
    setup.state.sampling_policy = Arc::new(
        openai_server::sampling_policy::InMemorySamplingPolicyStore::new(
            openai_server::sampling_policy::SamplingPolicy {
                enabled: true,
                default_rate: 0.0,
                default_log_content: false,
                ..Default::default()
            },
        ),
    );
    setup
        .state
        .sampling_gate
        .set_store(setup.state.sampling_policy.clone());
    setup.state.sampling_gate.refresh().await;
    let accepted = accounting_app(setup.state.clone())
        .oneshot(request(
            "/v1/responses",
            serde_json::json!({
                "model":"openai/gpt-5-codex","input":"task","background":true,"max_output_tokens":32
            }),
        ))
        .await
        .unwrap();
    let status = accepted.status();
    let accepted_bytes = body_bytes(accepted).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&accepted_bytes)
    );
    let accepted: serde_json::Value = serde_json::from_slice(&accepted_bytes).unwrap();
    assert_eq!(accepted["status"], "queued");
    sqlx::query("CREATE FUNCTION chalk_router.background_quota_test_usage_failure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.environment_id LIKE 'background-atomic-%' THEN RAISE EXCEPTION 'simulated usage insert failure'; END IF; RETURN NEW; END $$")
        .execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER background_quota_test_usage_failure BEFORE INSERT ON chalk_router.usage_events FOR EACH ROW EXECUTE FUNCTION chalk_router.background_quota_test_usage_failure()")
        .execute(&pool).await.unwrap();
    setup
        .state
        .background_responses
        .reconcile(&setup.state)
        .await
        .unwrap();
    let phase: String = sqlx::query_scalar(
        "SELECT phase FROM chalk_router.background_response_jobs WHERE environment_id=$1",
    )
    .bind(&environment_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(phase, "terminal");
    let active: i64 =
        sqlx::query_scalar("SELECT active_count FROM chalk_router.quota_states WHERE policy_id=$1")
            .bind(&concurrency.quota_policy_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(active, 1);
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM chalk_router.usage_events WHERE environment_id=$1",
    )
    .bind(&environment_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(events, 0);
    sqlx::query("DROP TRIGGER background_quota_test_usage_failure ON chalk_router.usage_events")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION chalk_router.background_quota_test_usage_failure()")
        .execute(&pool)
        .await
        .unwrap();
    setup.state.background_responses = runtime();
    setup.state.quota_backend = Arc::new(PostgresQuota::new(pool.clone()));
    let service = setup.state.background_response_service();
    let terminal = service.list(Some("terminal"), None, None).await.unwrap();
    assert_eq!(terminal.jobs.len(), 1);
    assert!(terminal.jobs[0].last_error_code.is_some());
    sqlx::query("UPDATE chalk_router.background_response_jobs SET next_poll_at=now() WHERE environment_id=$1")
        .bind(&environment_id).execute(&pool).await.unwrap();
    // A second instance races the claim the way another replica would; one
    // instance serializes its own reconciles.
    let replica = runtime();
    let (first, second) = tokio::join!(
        setup.state.background_responses.reconcile(&setup.state),
        replica.reconcile(&setup.state)
    );
    first.unwrap();
    second.unwrap();
    assert_eq!(
        setup.backend.retrieve_calls().len(),
        1,
        "persisted terminal usage avoids another upstream poll"
    );
    let phase: String = sqlx::query_scalar(
        "SELECT phase FROM chalk_router.background_response_jobs WHERE environment_id=$1",
    )
    .bind(&environment_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(phase, "settled");
    let used: i64 =
        sqlx::query_scalar("SELECT used_amount FROM chalk_router.quota_states WHERE policy_id=$1")
            .bind(&budget.quota_policy_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(used, 5);
    let active: i64 =
        sqlx::query_scalar("SELECT active_count FROM chalk_router.quota_states WHERE policy_id=$1")
            .bind(&concurrency.quota_policy_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(active, 0);
    let owned_entries:i64=sqlx::query_scalar("SELECT (SELECT count(*) FROM chalk_router.quota_reservations WHERE background_environment_id=$1)+(SELECT count(*) FROM chalk_router.quota_leases WHERE background_environment_id=$1)").bind(&environment_id).fetch_one(&pool).await.unwrap();
    assert_eq!(owned_entries, 0);
    sqlx::query("UPDATE chalk_router.background_response_jobs SET settled_at=now()-interval '31 days' WHERE environment_id=$1").bind(&environment_id).execute(&pool).await.unwrap();
    let replica = runtime();
    let (pruned, reconciled, replica_pruned) = tokio::join!(
        setup.state.background_responses.cleanup(),
        replica.reconcile(&setup.state),
        replica.cleanup()
    );
    assert_eq!(pruned.unwrap() + replica_pruned.unwrap(), 1);
    reconciled.unwrap();
    let after:(i64,i64)=sqlx::query_as("SELECT (SELECT used_amount FROM chalk_router.quota_states WHERE policy_id=$1),(SELECT active_count FROM chalk_router.quota_states WHERE policy_id=$2)").bind(&budget.quota_policy_id).bind(&concurrency.quota_policy_id).fetch_one(&pool).await.unwrap();
    assert_eq!(after, (5, 0));
    let replay = accounting_app(setup.state.clone())
        .oneshot(response_operation_request(
            "GET",
            accepted["id"].as_str().unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::OK);
    let events:(i64,i64)=sqlx::query_as("SELECT count(*),sum(total_tokens)::bigint FROM chalk_router.usage_events WHERE environment_id=$1").bind(&environment_id).fetch_one(&pool).await.unwrap();
    assert_eq!(events, (1, 5));
}

#[tokio::test]
async fn postgres_background_listing_and_retention_preserve_owners_and_environment_isolation() {
    let Some(url) = crate::support::db::test_database_url("postgres_background_operations") else {
        return;
    };
    let _lock = crate::support::db::connect_locked(&url).await;
    openai_server::migrate::run(&url).await.unwrap();
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let environment_id = format!("background-operations-{}", uuid::Uuid::new_v4());
    let mut setup = setup(ApiKeyPermission::default(), FallbackPolicy::default()).await;
    setup.state.environment_id = environment_id.clone();
    setup.state.backend = Arc::new(MockBackend {
        background_queued: true,
        ..Default::default()
    });
    let runtime = Arc::new(
        openai_server::background_responses::BackgroundResponses::new(
            Some(pool.clone()),
            environment_id.clone(),
            Default::default(),
        ),
    );
    setup.state.background_responses = runtime.clone();
    let accepted = accounting_app(setup.state.clone())
        .oneshot(request(
            "/v1/responses",
            serde_json::json!({"model":"openai/gpt-5-codex","input":"task","background":true}),
        ))
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);
    let template: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM chalk_router.background_response_jobs WHERE environment_id=$1",
    )
    .bind(&environment_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let mut owned = Vec::new();
    for index in 0..110 {
        let event_id = uuid::Uuid::new_v4();
        let phase = match index {
            100..=101 | 103 => "submitting",
            104 => "active",
            105 => "terminal",
            106..=107 => "unresolved",
            _ => "settled",
        };
        let mut payload = template.clone();
        payload["event_id"] = serde_json::json!(event_id);
        payload["phase"] = serde_json::json!(phase);
        payload["submitted_at"] =
            serde_json::json!(chrono::Utc::now() - chrono::Duration::days(100));
        let upstream_id = format!("resp_{event_id}");
        payload["upstream_response_id"] = if index == 106 {
            serde_json::Value::Null
        } else {
            serde_json::json!(upstream_id)
        };
        if index == 107 {
            payload["submission_rejected"] = serde_json::json!(true);
        }
        let settled_at = (phase == "settled").then(|| {
            chrono::Utc::now() - chrono::Duration::days(if index == 102 { 1 } else { 31 })
        });
        let claim_until = (index == 109).then(|| chrono::Utc::now() + chrono::Duration::hours(1));
        sqlx::query("INSERT INTO chalk_router.background_response_jobs (environment_id,event_id,phase,payload,connection_id,upstream_response_id,next_poll_at,settled_at,claim_until,created_at) VALUES ($1,$2,$3,$4,$5,$6,now()+interval '1 hour',$7,$8,'2026-01-01'::timestamptz)")
            .bind(&environment_id).bind(event_id).bind(phase).bind(&payload).bind(payload["connection_id"].as_str())
            .bind(payload["upstream_response_id"].as_str()).bind(settled_at).bind(claim_until).execute(&pool).await.unwrap();
        if matches!(index, 100 | 101) {
            owned.push(event_id);
        }
    }
    let quota = PostgresQuota::new(pool.clone());
    let policy_store = PostgresRateLimitPolicyStore::new(pool.clone(), environment_id.clone());
    for (index, event_id) in owned.iter().enumerate() {
        let policy = policy_store
            .create(RateLimitPolicy {
                quota_policy_id: String::new(),
                id: uuid::Uuid::new_v4().to_string(),
                limit_type: if index == 0 {
                    "tokens_per_minute"
                } else {
                    "concurrent_requests"
                }
                .to_string(),
                limit_value: 1000,
                burst_value: (index == 0).then_some(1000),
                connection_id: None,
                models: Vec::new(),
                scope: "api_key".to_string(),
                scope_id: template["key_hash"].as_str().unwrap().to_string(),
                enabled: true,
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
        let action = QuotaAction {
            policy_id: policy.quota_policy_id,
            environment_id: environment_id.clone(),
            source: openai_server::quota::QuotaSource::RateLimit,
            algorithm: if index == 0 {
                openai_server::quota::QuotaAlgorithm::TokenBucket
            } else {
                openai_server::quota::QuotaAlgorithm::Concurrency
            },
            meter: if index == 0 {
                openai_server::quota::QuotaMeter::Tokens
            } else {
                openai_server::quota::QuotaMeter::ConcurrentRequests
            },
            limit_value: 1000,
            burst: (index == 0).then_some(1000),
            amount: if index == 0 { 10 } else { 1 },
            window_start: Some(chrono::Utc::now()),
            window_end: Some(chrono::Utc::now() + chrono::Duration::days(1)),
        };
        let decision = quota
            .acquire_background(
                &[action],
                None,
                &openai_server::quota::BackgroundOwner {
                    environment_id: environment_id.clone(),
                    event_id: *event_id,
                },
            )
            .await
            .unwrap();
        assert!(matches!(decision, QuotaDecision::Allowed(Some(_))));
        sqlx::query("UPDATE chalk_router.background_response_jobs SET phase='settled',payload=jsonb_set(payload,'{phase}','\"settled\"'),settled_at=now()-interval '31 days' WHERE environment_id=$1 AND event_id=$2")
            .bind(&environment_id).bind(event_id).execute(&pool).await.unwrap();
    }
    let service = setup.state.background_response_service();
    let first = service
        .list(Some("settled"), None, Some(5000))
        .await
        .unwrap();
    assert_eq!(first.jobs.len(), 100);
    let second = service
        .list(Some("settled"), first.next_cursor.as_deref(), Some(100))
        .await
        .unwrap();
    assert_eq!(second.jobs.len(), 5);
    let ids: std::collections::HashSet<_> = first
        .jobs
        .iter()
        .chain(&second.jobs)
        .map(|job| job.event_id)
        .collect();
    assert_eq!(ids.len(), 105);
    sqlx::query("INSERT INTO chalk_router.background_response_jobs (environment_id,event_id,phase,payload,next_poll_at,settled_at) SELECT $2,event_id,phase,payload,next_poll_at,settled_at FROM chalk_router.background_response_jobs WHERE environment_id=$1 AND phase='settled' AND settled_at<now()-interval '30 days' LIMIT 1")
        .bind(&environment_id).bind(format!("other-{environment_id}")).execute(&pool).await.unwrap();
    let other = openai_server::service::BackgroundResponseService::new(Arc::new(
        openai_server::background_responses::BackgroundResponses::new(
            Some(pool.clone()),
            format!("other-{environment_id}"),
            Default::default(),
        ),
    ));
    assert!(other.list(None, None, None).await.unwrap().jobs.is_empty());
    let mut locked = pool.begin().await.unwrap();
    sqlx::query("SELECT event_id FROM chalk_router.background_response_jobs AS job WHERE environment_id=$1 AND phase='settled' AND settled_at<now()-interval '30 days' AND claim_until IS NULL AND NOT EXISTS (SELECT 1 FROM chalk_router.quota_reservations WHERE background_environment_id=job.environment_id AND background_event_id=job.event_id) AND NOT EXISTS (SELECT 1 FROM chalk_router.quota_leases WHERE background_environment_id=job.environment_id AND background_event_id=job.event_id) LIMIT 1 FOR UPDATE")
        .bind(&environment_id).fetch_one(&mut *locked).await.unwrap();
    assert_eq!(runtime.cleanup().await.unwrap(), 100);
    assert_eq!(runtime.cleanup().await.unwrap(), 0);
    locked.rollback().await.unwrap();
    assert_eq!(runtime.cleanup().await.unwrap(), 1);
    assert_eq!(
        service
            .list(Some("settled"), None, None)
            .await
            .unwrap()
            .jobs
            .len(),
        4
    );
    assert_eq!(service.list(None, None, None).await.unwrap().jobs.len(), 6);
    let owners:i64=sqlx::query_scalar("SELECT (SELECT count(*) FROM chalk_router.quota_reservations WHERE background_environment_id=$1)+(SELECT count(*) FROM chalk_router.quota_leases WHERE background_environment_id=$1)").bind(&environment_id).fetch_one(&pool).await.unwrap();
    assert_eq!(owners, 2);
    for event_id in owned {
        quota
            .finish_background(
                &openai_server::quota::BackgroundOwner {
                    environment_id: environment_id.clone(),
                    event_id,
                },
                QuotaOutcome::Unknown,
            )
            .await
            .unwrap();
    }
    assert_eq!(runtime.cleanup().await.unwrap(), 2);
    assert_eq!(
        other
            .list(Some("settled"), None, None)
            .await
            .unwrap()
            .jobs
            .len(),
        1
    );
}

impl Default for MockBackend {
    fn default() -> Self {
        Self {
            background_queued: false,
            submission_ambiguous: false,
            submission_pending: false,
            submission_delay: None,
            retrieve_gate: None,
            chat_calls: Arc::new(Mutex::new(Vec::new())),
            response_calls: Arc::new(Mutex::new(Vec::new())),
            stream_calls: Arc::new(Mutex::new(Vec::new())),
            compact_calls: Arc::new(Mutex::new(Vec::new())),
            retrieve_calls: Arc::new(Mutex::new(Vec::new())),
            retrieve_stream_calls: Arc::new(Mutex::new(Vec::new())),
            cancel_calls: Arc::new(Mutex::new(Vec::new())),
            delete_calls: Arc::new(Mutex::new(Vec::new())),
            resolve_calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl MockBackend {
    /// `chalk/<name>` resolves as model route `route-<name>` on an OpenAI
    /// connection.
    fn resolution(model: &str) -> RouteResolution {
        let (provider, model_route_id) = match model.split_once('/') {
            Some(("chalk", name)) => ("openai", Some(format!("route-{name}"))),
            Some((provider, _)) => (provider, None),
            None => (model, None),
        };
        RouteResolution {
            provider_kind: provider.to_string(),
            connection_id: Some("connection-1".to_string()),
            connection_name: provider.to_string(),
            upstream_model_id: model.to_string(),
            connection_prefix: None,
            model_route_id,
        }
    }

    fn resolved_route(&self, model: &str) -> ResolvedRoute {
        self.resolve_calls.fetch_add(1, Ordering::SeqCst);
        ResolvedRoute {
            routing: Default::default(),
            backend: Arc::new(self.clone()),
            stripped_model: model.to_string(),
            capabilities: Arc::new(
                openai_server::capabilities::resolve(
                    if model.contains("incompatible") {
                        "voyage"
                    } else {
                        match model.split_once('/') {
                            Some(("chalk", _)) | None => "openai",
                            Some((provider, _)) => provider,
                        }
                    },
                    model,
                    None,
                    None,
                )
                .unwrap(),
            ),
            resolution: Self::resolution(model),
        }
    }

    fn response_calls(&self) -> Vec<serde_json::Value> {
        self.response_calls.lock().unwrap().clone()
    }

    fn chat_calls(&self) -> Vec<ChatCompletionRequest> {
        self.chat_calls.lock().unwrap().clone()
    }

    fn stream_calls(&self) -> Vec<serde_json::Value> {
        self.stream_calls.lock().unwrap().clone()
    }

    fn compact_calls(&self) -> Vec<serde_json::Value> {
        self.compact_calls.lock().unwrap().clone()
    }

    fn retrieve_calls(&self) -> Vec<String> {
        self.retrieve_calls.lock().unwrap().clone()
    }

    fn retrieve_stream_calls(&self) -> Vec<(String, Option<u64>)> {
        self.retrieve_stream_calls.lock().unwrap().clone()
    }

    fn cancel_calls(&self) -> Vec<String> {
        self.cancel_calls.lock().unwrap().clone()
    }

    fn delete_calls(&self) -> Vec<String> {
        self.delete_calls.lock().unwrap().clone()
    }

    fn success(model: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "resp_integration",
            "object": "response",
            "created_at": 1_700_000_000,
            "status": "completed",
            "model": model,
            "output": [{
                "id": "msg_integration",
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": format!("served-by:{model}"),
                    "annotations": []
                }]
            }],
            "usage": {
                "input_tokens": 3,
                "input_tokens_details": {"cached_tokens": 1},
                "output_tokens": 2,
                "output_tokens_details": {"reasoning_tokens": 1},
                "total_tokens": 5
            },
            "codex_marker": "unknown-response-field-preserved"
        })
    }

    fn dispatch_failure(model: &str) -> Option<BackendError> {
        if model.contains("bad-upstream-model") {
            Some(BackendError::api_status(
                400,
                "the requested upstream model does not exist",
            ))
        } else {
            model.contains("ratelimit").then(|| {
                BackendError::api_error(
                    "slow down",
                    429,
                    vec![("retry-after".to_string(), "17".to_string())],
                )
            })
        }
    }
}

#[async_trait]
impl BackendProvider for MockBackend {
    async fn create_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        let model = request.model.clone();
        self.chat_calls.lock().unwrap().push(request);
        Ok(ChatCompletionResponse {
            id: "chatcmpl_integration".to_string(),
            object: "chat.completion".to_string(),
            created: 1_700_000_000,
            model,
            choices: vec![ChatChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: Some(serde_json::json!("translated response")),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(Usage {
                prompt_tokens: 3,
                completion_tokens: Some(2),
                total_tokens: 5,
                ..Usage::default()
            }),
        })
    }

    async fn create_response(
        &self,
        request: ResponsesRequest,
        _headers: &ResponsesRequestHeaders,
    ) -> Result<BackendJsonResponse, BackendError> {
        let request = serde_json::to_value(request).unwrap();
        self.response_calls.lock().unwrap().push(request.clone());
        if self.submission_pending {
            return std::future::pending().await;
        }
        if let Some(delay) = self.submission_delay {
            tokio::time::sleep(delay).await;
        }
        if self.submission_ambiguous {
            return Err(BackendError::Other("submission connection was lost".into()));
        }
        let model = request["model"].as_str().unwrap();
        if let Some(error) = Self::dispatch_failure(model) {
            return Err(error);
        }
        BackendJsonResponse::from_value(
            {
                let mut response = Self::success(model);
                if self.background_queued && request["background"] == true {
                    response["status"] = "queued".into();
                    response.as_object_mut().unwrap().remove("usage");
                }
                response
            },
            vec![(
                "x-request-id".to_string(),
                "upstream-response-id".to_string(),
            )],
        )
    }

    async fn create_response_stream(
        &self,
        request: ResponsesRequest,
        _headers: &ResponsesRequestHeaders,
    ) -> Result<BackendSseResponse, BackendError> {
        let request = serde_json::to_value(request).unwrap();
        self.stream_calls.lock().unwrap().push(request.clone());
        if self.submission_ambiguous {
            return Err(BackendError::Other("submission connection was lost".into()));
        }
        let model = request["model"].as_str().unwrap();
        if let Some(error) = Self::dispatch_failure(model) {
            return Err(error);
        }

        let created = serde_json::json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": {
                "id": "resp_stream_integration",
                "object": "response",
                "status": "in_progress",
                "model": model,
                "output": []
            }
        });
        let mut terminal_response = Self::success(model);
        terminal_response["id"] = "resp_stream_integration".into();
        let completed = serde_json::json!({
            "type": "response.completed",
            "sequence_number": 1,
            "response": terminal_response
        });
        let bytes = bytes::Bytes::from(
            if self.background_queued && request["background"] == true {
                format!("event: response.created\r\ndata: {created}\r\n\r\n")
            } else {
                format!("event: response.created\r\ndata: {created}\r\n\r\nevent: response.completed\ndata: {completed}\n\n")
            },
        );
        let stream: SseByteStream = Box::pin(futures::stream::once(async move { Ok(bytes) }));
        Ok(BackendSseResponse {
            stream,
            headers: vec![("x-request-id".to_string(), "upstream-stream-id".to_string())],
            status: 200,
            content_type: "text/event-stream".into(),
        })
    }

    async fn compact_response(
        &self,
        request: openai_server::schema_json::SchemaJson,
        _headers: &ResponsesRequestHeaders,
    ) -> Result<BackendJsonResponse, BackendError> {
        self.compact_calls
            .lock()
            .unwrap()
            .push(request.value().clone());
        if request["simulate_failure"] == true {
            return Err(BackendError::api_status(503, "mock compaction unavailable"));
        }
        let model = request["model"].as_str().unwrap();
        BackendJsonResponse::from_value(
            serde_json::json!({
                "id": "resp_compact_integration",
                "object": "response.compaction",
                "created_at": 1_700_000_001,
                "model": model,
                "output": [{
                    "type": "compaction",
                    "id": "cmp_integration",
                    "encrypted_content": "opaque-compaction-item"
                }]
            }),
            Vec::new(),
        )
    }

    async fn retrieve_response(
        &self,
        response_id: &str,
        _headers: &ResponsesRequestHeaders,
    ) -> Result<BackendJsonResponse, BackendError> {
        self.retrieve_calls
            .lock()
            .unwrap()
            .push(response_id.to_string());
        let _permit = match &self.retrieve_gate {
            Some(gate) => Some(gate.acquire().await.unwrap()),
            None => None,
        };
        BackendJsonResponse::from_value(
            {
                let mut response = Self::success("openai/gpt-5-codex");
                response["id"] = response_id.into();
                response
            },
            Vec::new(),
        )
    }

    async fn retrieve_response_stream(
        &self,
        response_id: &str,
        starting_after: Option<u64>,
        _headers: &ResponsesRequestHeaders,
    ) -> Result<BackendSseResponse, BackendError> {
        self.retrieve_stream_calls
            .lock()
            .unwrap()
            .push((response_id.to_string(), starting_after));
        let resumed = serde_json::json!({
            "type": "response.output_text.delta",
            "sequence_number": 43,
            "response_id": response_id,
            "delta": "resumed"
        });
        let completed = serde_json::json!({
            "type": "response.completed",
            "sequence_number": 44,
            "response": Self::success("openai/gpt-5-codex")
        });
        let bytes = bytes::Bytes::from(format!(
            "event: response.output_text.delta\ndata: {resumed}\n\nevent: response.completed\ndata: {completed}\n\n"
        ));
        Ok(BackendSseResponse {
            stream: Box::pin(futures::stream::once(async move { Ok(bytes) })),
            headers: vec![("x-request-id".to_string(), "resume-request-id".to_string())],
            status: 200,
            content_type: "text/event-stream".into(),
        })
    }

    async fn cancel_response(
        &self,
        response_id: &str,
        _headers: &ResponsesRequestHeaders,
    ) -> Result<BackendJsonResponse, BackendError> {
        self.cancel_calls
            .lock()
            .unwrap()
            .push(response_id.to_string());
        BackendJsonResponse::from_value(
            serde_json::json!({
                "id": response_id,
                "object": "response",
                "status": "cancelled",
                "output": []
            }),
            Vec::new(),
        )
    }

    async fn delete_response(
        &self,
        response_id: &str,
        _headers: &ResponsesRequestHeaders,
    ) -> Result<BackendJsonResponse, BackendError> {
        self.delete_calls
            .lock()
            .unwrap()
            .push(response_id.to_string());
        BackendJsonResponse::from_value(
            serde_json::json!({
                "id": response_id,
                "object": "response.deleted",
                "deleted": true
            }),
            Vec::new(),
        )
    }

    async fn create_embedding(
        &self,
        _request: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }

    async fn create_image(
        &self,
        _request: CreateImageRequest,
    ) -> Result<ImageResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }

    async fn create_speech(
        &self,
        _request: CreateSpeechRequest,
    ) -> Result<bytes::Bytes, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }

    async fn create_transcription(
        &self,
        _model: String,
        _file_bytes: bytes::Bytes,
        _file_name: String,
        _language: Option<String>,
        _prompt: Option<String>,
        _response_format: Option<String>,
        _temperature: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }

    async fn create_translation(
        &self,
        _model: String,
        _file_bytes: bytes::Bytes,
        _file_name: String,
        _prompt: Option<String>,
        _response_format: Option<String>,
        _temperature: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }

    async fn create_moderation(
        &self,
        _request: CreateModerationRequest,
    ) -> Result<CreateModerationResponse, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }

    async fn list_models(&self) -> Result<ListResponse<Model>, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }

    async fn get_model(&self, _model_id: &str) -> Result<Model, BackendError> {
        Err(BackendError::Other("not implemented".into()))
    }

    fn name(&self) -> &str {
        "responses-test"
    }

    async fn resolve(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        Ok(self.resolved_route(model))
    }

    async fn resolve_strict(&self, model: &str) -> Result<ResolvedRoute, BackendError> {
        Ok(self.resolved_route(model))
    }

    async fn resolve_connection(&self, connection_id: &str) -> Result<ResolvedRoute, BackendError> {
        assert_eq!(connection_id, "connection-1");
        Ok(self.resolved_route("openai/gpt-5-codex"))
    }
}

struct Setup {
    backend: Arc<MockBackend>,
    app: Router,
    state: AppState,
}

async fn state_with(
    backend: Arc<MockBackend>,
    api_key_store: Arc<dyn ApiKeyStore>,
    fallbacks: FallbackPolicy,
) -> AppState {
    AppStateBuilder::new(backend)
        .environment_id("test")
        .api_key_store(api_key_store)
        .fallback_policy(fallbacks)
        .build()
        .await
}

async fn setup(permission: ApiKeyPermission, fallbacks: FallbackPolicy) -> Setup {
    setup_with_budget(permission, fallbacks, None).await
}

async fn setup_with_budget(
    permission: ApiKeyPermission,
    fallbacks: FallbackPolicy,
    budget_amount: Option<u64>,
) -> Setup {
    setup_with_quota(permission, fallbacks, budget_amount, None).await
}

async fn setup_with_quota(
    permission: ApiKeyPermission,
    fallbacks: FallbackPolicy,
    budget_amount: Option<u64>,
    rate_limit: Option<(&str, Option<&str>)>,
) -> Setup {
    setup_with_quota_backend(
        permission,
        fallbacks,
        budget_amount,
        rate_limit,
        Arc::new(MockBackend::default()),
    )
    .await
}

async fn setup_with_quota_backend(
    permission: ApiKeyPermission,
    fallbacks: FallbackPolicy,
    budget_amount: Option<u64>,
    rate_limit: Option<(&str, Option<&str>)>,
    backend: Arc<MockBackend>,
) -> Setup {
    let hasher = KeyHasher::new(b"responses-test-pepper".to_vec());
    let key_hash = hasher.hash(TEST_KEY);
    let key_store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(hasher.clone()));
    key_store
        .insert_raw(openai_server::api_keys::ApiKey {
            key_hash: key_hash.clone(),
            key_prefix: openai_server::hashing::key_prefix(TEST_KEY),
            description: "Responses integration key".to_string(),
            created_at: chrono::Utc::now(),
            permission,
            labels: HashMap::new(),
            usage_pool_id: None,
            daily_token_budget: None,
        })
        .await
        .expect("seed key");

    let mut state = state_with(backend.clone(), key_store, fallbacks).await;
    if let Some(limit_amount) = budget_amount {
        let budget_store = Arc::new(MemoryBudgetPolicyStore::new("test".to_string()));
        budget_store
            .insert(BudgetPolicy {
                quota_policy_id: "compact-budget".to_string(),
                id: "compact-budget-source".to_string(),
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
        state.budget_store = budget_store;
    }
    if budget_amount.is_some() || rate_limit.is_some() {
        let rate_store = Arc::new(MemoryRateLimitPolicyStore::new("test".to_string()));
        if let Some((limit_type, connection_id)) = rate_limit {
            rate_store
                .create(RateLimitPolicy {
                    quota_policy_id: "responses-history-limit".into(),
                    id: "responses-history-limit-source".into(),
                    limit_type: limit_type.into(),
                    limit_value: if limit_type == "concurrent_requests" {
                        1
                    } else {
                        100_000
                    },
                    burst_value: None,
                    connection_id: connection_id.map(str::to_string),
                    models: if connection_id.is_some() {
                        vec!["openai/gpt-5-codex".into()]
                    } else {
                        vec![]
                    },
                    scope: "api_key".into(),
                    scope_id: key_hash,
                    enabled: true,
                    created_at: chrono::Utc::now(),
                })
                .await
                .unwrap();
        }
        state.rate_limit_store = rate_store.clone();
        state.quota_resolver = Arc::new(openai_server::quota_policy::QuotaPolicyResolver::new(
            "test".to_string(),
            state.budget_store.clone(),
            rate_store,
        ));
        state.quota_backend = Arc::new(MemoryQuota::new());
    }
    let app = Router::new()
        .route(
            "/v1/responses",
            post(openai_server::handlers::responses::create_response),
        )
        .route(
            "/v1/responses/compact",
            post(openai_server::handlers::responses::compact_response),
        )
        .route(
            "/v1/responses/:response_id",
            axum::routing::get(openai_server::handlers::responses::retrieve_response)
                .delete(openai_server::handlers::responses::delete_response),
        )
        .route(
            "/v1/responses/:response_id/cancel",
            post(openai_server::handlers::responses::cancel_response),
        )
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));
    Setup {
        backend,
        app,
        state,
    }
}

/// Wraps [`MemoryQuota`] to count every reconciliation (`QuotaPermit::finish`)
/// the quota backend observes, so a test can assert settlement happened
/// exactly once through the shared `with_quota_settlement` helper rather than
/// being skipped (a leaked permit) or double-settled.
#[derive(Default)]
struct FinishRecordingQuota {
    inner: MemoryQuota,
    acquisitions: AtomicUsize,
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
        self.acquisitions.fetch_add(1, Ordering::SeqCst);
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

/// Like [`setup`], but installs a `concurrent_requests` rate limit (no budget
/// policy) backed by [`FinishRecordingQuota`], so the plain (non-compact)
/// create-response path reserves and settles exactly one quota grant per
/// request through [`try_with_fallbacks_if`](openai_server::handlers).
async fn setup_with_recording_quota(
    permission: ApiKeyPermission,
    fallbacks: FallbackPolicy,
) -> (Setup, Arc<FinishRecordingQuota>) {
    let backend = Arc::new(MockBackend::default());
    let hasher = KeyHasher::new(b"responses-test-pepper".to_vec());
    let key_hash = hasher.hash(TEST_KEY);
    let key_store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(hasher.clone()));
    key_store
        .insert_raw(openai_server::api_keys::ApiKey {
            key_hash: key_hash.clone(),
            key_prefix: openai_server::hashing::key_prefix(TEST_KEY),
            description: "Responses integration key".to_string(),
            created_at: chrono::Utc::now(),
            permission,
            labels: HashMap::new(),
            usage_pool_id: None,
            daily_token_budget: None,
        })
        .await
        .expect("seed key");

    let mut state = state_with(backend.clone(), key_store, fallbacks).await;
    let rate_store = Arc::new(MemoryRateLimitPolicyStore::new("test".to_string()));
    rate_store
        .create(RateLimitPolicy {
            quota_policy_id: "responses-usage-concurrency".to_string(),
            id: "responses-usage-concurrency-source".to_string(),
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
    let quota_backend = Arc::new(FinishRecordingQuota::default());
    state.rate_limit_store = rate_store.clone();
    state.quota_resolver = Arc::new(openai_server::quota_policy::QuotaPolicyResolver::new(
        "test".to_string(),
        state.budget_store.clone(),
        rate_store,
    ));
    state.quota_backend = quota_backend.clone();

    let app = Router::new()
        .route(
            "/v1/responses",
            post(openai_server::handlers::responses::create_response),
        )
        .route(
            "/v1/responses/compact",
            post(openai_server::handlers::responses::compact_response),
        )
        .route(
            "/v1/responses/:response_id",
            axum::routing::get(openai_server::handlers::responses::retrieve_response)
                .delete(openai_server::handlers::responses::delete_response),
        )
        .route(
            "/v1/responses/:response_id/cancel",
            post(openai_server::handlers::responses::cancel_response),
        )
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));
    (
        Setup {
            backend,
            app,
            state,
        },
        quota_backend,
    )
}

fn policy(primary: &str, alternates: &[&str]) -> FallbackPolicy {
    let mut map = HashMap::new();
    map.insert(
        primary.to_string(),
        alternates.iter().map(|model| model.to_string()).collect(),
    );
    FallbackPolicy::from_map(map)
}

fn request(path: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn response_operation_request(method: &str, response_id: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(format!("/v1/responses/{response_id}"))
        .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
        .body(Body::empty())
        .unwrap()
}

async fn body_bytes(response: axum::response::Response) -> bytes::Bytes {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body")
}

#[tokio::test]
async fn unclassified_tools_require_absence_of_matching_token_limits_and_provider_input_soft_caps()
{
    use serde_json::json;

    for policy in [
        "budget",
        "global_tpm",
        "route_tpm",
        "other_route_tpm",
        "rpm",
        "concurrency",
        "none",
    ] {
        let requires_estimate = matches!(policy, "budget" | "global_tpm" | "route_tpm");
        let (budget, rate) = match policy {
            "budget" => (Some(100_000), None),
            "global_tpm" => (None, Some(("tokens_per_minute", None))),
            "route_tpm" => (None, Some(("tokens_per_minute", Some("connection-1")))),
            "other_route_tpm" => (None, Some(("tokens_per_minute", Some("other-connection")))),
            "rpm" => (None, Some(("requests_per_minute", None))),
            "concurrency" => (None, Some(("concurrent_requests", None))),
            _ => (None, None),
        };
        for streaming in [false, true] {
            let setup = setup_with_quota(
                ApiKeyPermission::default(),
                FallbackPolicy::default(),
                budget,
                rate,
            )
            .await;
            let calls =
                || setup.backend.response_calls().len() + setup.backend.stream_calls().len();
            let locator = openai_server::response_locator::ResponseLocatorCodec::new(
                b"test-locator".to_vec(),
            )
            .encode("connection-1", None, "resp_previous");
            for (patch, unknown_tokens) in [
                // Stored history and opaque content are soft caps over the
                // visible input, like hosted tools.
                (json!({"previous_response_id":"resp_previous"}), false),
                (json!({"previous_response_id":locator}), false),
                (json!({"conversation":"conv_previous"}), false),
                (json!({"conversation":{"id":"conv_previous"}}), false),
                (
                    json!({"input":[{"type":"item_reference","id":"item_previous"}]}),
                    false,
                ),
                (json!({"input":[{"id":"item_previous"}]}), false),
                (
                    json!({"input":[
                        {"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"gAAAAopaque"},
                        {"role":"user","content":"Continue"}
                    ]}),
                    false,
                ),
                (
                    json!({"input":[{"role":"user","content":[
                        {"type":"input_text","text":"Describe"},
                        {"type":"input_image","image_url":"data:image/png;base64,iVBORw0KGgo="}
                    ]}]}),
                    false,
                ),
                (json!({"tools":[{"type":"future_hosted_tool"}]}), true),
                // Hosted tools are a soft cap, not an unknown estimate.
                (json!({"tools":[{"type":"web_search"}]}), false),
                (json!({"tools":[{"type":"file_search"}]}), false),
                (json!({"tools":[{"type":"code_interpreter"}]}), false),
                (
                    json!({"tools":[{"type":"function","name":"lookup","parameters":{"type":"object","properties":{}}}]}),
                    false,
                ),
                (
                    json!({"tools":[{"type":"custom","name":"lookup","format":{"type":"text"}}]}),
                    false,
                ),
                (
                    json!({"previous_response_id":null,"conversation":null}),
                    false,
                ),
                (
                    json!({"input":[
                        {"role":"user","content":"Weather in Paris?"},
                        {"type":"function_call","call_id":"call-1","name":"weather","arguments":"{}"},
                        {"type":"function_call_output","call_id":"call-1","output":"Sunny"}
                    ]}),
                    false,
                ),
            ] {
                let mut body = json!({"model":"openai/gpt-5-codex","input":"Continue",
                    "stream":streaming,"max_output_tokens":100});
                body.as_object_mut()
                    .unwrap()
                    .extend(patch.as_object().unwrap().clone());
                let before = calls();
                let response = setup
                    .app
                    .clone()
                    .oneshot(request("/v1/responses", body.clone()))
                    .await
                    .unwrap();
                let rejected = requires_estimate && unknown_tokens;
                assert_eq!(
                    response.status(),
                    if rejected {
                        StatusCode::BAD_REQUEST
                    } else {
                        StatusCode::OK
                    },
                    "policy={policy}, stream={streaming}, request={body}"
                );
                let bytes = body_bytes(response).await;
                if rejected {
                    assert!(String::from_utf8_lossy(&bytes)
                        .contains("tool types the router does not recognize"));
                    assert_eq!(calls(), before, "must reject before dispatch");
                } else {
                    assert_eq!(calls(), before + 1);
                }
            }
            assert!(setup.backend.retrieve_calls().is_empty());
        }
    }
}

#[tokio::test]
async fn background_admits_matching_budgets_and_rate_limits() {
    for policy in [
        "budget",
        "global_rpm",
        "global_tpm",
        "global_concurrency",
        "route_rpm",
        "route_tpm",
        "route_concurrency",
    ] {
        let (budget, rate) = match policy {
            "budget" => (Some(100_000), None),
            "global_rpm" => (None, Some(("requests_per_minute", None))),
            "global_tpm" => (None, Some(("tokens_per_minute", None))),
            "global_concurrency" => (None, Some(("concurrent_requests", None))),
            "route_rpm" => (None, Some(("requests_per_minute", Some("connection-1")))),
            "route_tpm" => (None, Some(("tokens_per_minute", Some("connection-1")))),
            "route_concurrency" => (None, Some(("concurrent_requests", Some("connection-1")))),
            _ => unreachable!(),
        };
        let setup = setup_with_quota(
            ApiKeyPermission::default(),
            FallbackPolicy::default(),
            budget,
            rate,
        )
        .await;
        let response = setup
            .app
            .oneshot(request(
                "/v1/responses",
                serde_json::json!({
                    "model": "openai/gpt-5-codex",
                    "input": "long task",
                    "background": true,
                    "max_output_tokens": 32
                }),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK, "policy={policy}");
        let body: serde_json::Value = serde_json::from_slice(&body_bytes(response).await).unwrap();
        assert_eq!(body["usage"]["total_tokens"], 5);
        assert_eq!(setup.backend.response_calls().len(), 1);
        assert!(setup.backend.response_calls()[0]
            .get("background_accounting_id")
            .is_none());
        assert!(setup.backend.stream_calls().is_empty());
    }
}

#[tokio::test]
async fn background_fails_closed_for_backends_without_durable_ownership() {
    let (setup, quota) =
        setup_with_recording_quota(ApiKeyPermission::default(), FallbackPolicy::default()).await;
    let response = setup
        .app
        .oneshot(request(
            "/v1/responses",
            serde_json::json!({
                "model": "openai/gpt-5-codex",
                "input": "long task",
                "background": true
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(quota.acquisitions.load(Ordering::SeqCst), 0);
    assert_eq!(quota.finishes.load(Ordering::SeqCst), 0);
    assert!(setup.backend.response_calls().is_empty());
}

#[tokio::test]
async fn background_concurrency_outlives_http_and_does_not_block_lifecycle_calls() {
    for connection_id in [None, Some("connection-1")] {
        let setup = setup_with_quota_backend(
            ApiKeyPermission::default(),
            FallbackPolicy::default(),
            None,
            Some(("concurrent_requests", connection_id)),
            Arc::new(MockBackend {
                background_queued: true,
                ..MockBackend::default()
            }),
        )
        .await;
        let create = || {
            request(
                "/v1/responses",
                serde_json::json!({
                    "model":"openai/gpt-5-codex", "input":"long task", "background":true, "max_output_tokens":32
                }),
            )
        };
        let accepted = setup.app.clone().oneshot(create()).await.unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);
        let accepted: serde_json::Value =
            serde_json::from_slice(&body_bytes(accepted).await).unwrap();
        assert_eq!(accepted["status"], "queued");
        setup.state.quota_backend.cleanup(100).await.unwrap();
        let blocked = setup.app.clone().oneshot(create()).await.unwrap();
        assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(setup.backend.response_calls().len(), 1);
        let terminal = setup
            .app
            .clone()
            .oneshot(response_operation_request(
                "GET",
                accepted["id"].as_str().unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(terminal.status(), StatusCode::OK);
        let admitted = setup.app.oneshot(create()).await.unwrap();
        assert_eq!(admitted.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn background_unclassified_tools_require_no_token_policy_and_soft_caps_admit() {
    for (payload, unknown) in [
        (
            serde_json::json!({"tools":[{"type":"future_hosted_tool"}]}),
            true,
        ),
        (serde_json::json!({"conversation":"conv_hidden"}), false),
        (serde_json::json!({"tools":[{"type":"web_search"}]}), false),
    ] {
        for policy in ["budget", "tpm", "rpm"] {
            let setup = setup_with_quota(
                ApiKeyPermission::default(),
                FallbackPolicy::default(),
                (policy == "budget").then_some(100_000),
                match policy {
                    "tpm" => Some(("tokens_per_minute", None)),
                    "rpm" => Some(("requests_per_minute", None)),
                    _ => None,
                },
            )
            .await;
            let mut body = serde_json::json!({"model":"openai/gpt-5-codex","input":"task","background":true,"max_output_tokens":32});
            body.as_object_mut()
                .unwrap()
                .extend(payload.as_object().unwrap().clone());
            let response = setup
                .app
                .oneshot(request("/v1/responses", body))
                .await
                .unwrap();
            if policy == "rpm" || !unknown {
                assert_eq!(response.status(), StatusCode::OK, "{policy} {payload}");
            } else {
                assert_eq!(response.status(), StatusCode::BAD_REQUEST);
                assert!(setup.backend.response_calls().is_empty());
            }
        }
    }
}

#[tokio::test]
async fn background_forwards_natively_without_matching_policies() {
    for streaming in [false, true] {
        let setup = setup(ApiKeyPermission::default(), FallbackPolicy::default()).await;
        let response = setup
            .app
            .oneshot(request(
                "/v1/responses",
                serde_json::json!({
                    "model": "openai/gpt-5-codex",
                    "input": "long task",
                    "background": true,
                    "stream": streaming
                }),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(body_bytes(response).await.to_vec()).unwrap();
        assert!(body.contains("resp_chalk_1."));
        let calls = if streaming {
            setup.backend.stream_calls()
        } else {
            setup.backend.response_calls()
        };
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["background"], true);
    }
}

#[tokio::test]
async fn background_ambiguous_submission_never_falls_back_or_releases_concurrency() {
    for streaming in [false, true] {
        let setup = setup_with_quota_backend(
            ApiKeyPermission::default(),
            policy("openai/gpt-5-codex", &["openai/gpt-4o"]),
            None,
            Some(("concurrent_requests", None)),
            Arc::new(MockBackend {
                submission_ambiguous: true,
                ..MockBackend::default()
            }),
        )
        .await;
        let create = || {
            request(
                "/v1/responses",
                serde_json::json!({"model":"openai/gpt-5-codex","input":"task","background":true,"stream":streaming,"max_output_tokens":32}),
            )
        };
        let failure = setup.app.clone().oneshot(create()).await.unwrap();
        assert_eq!(failure.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let blocked = setup.app.oneshot(create()).await.unwrap();
        assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            setup.backend.response_calls().len() + setup.backend.stream_calls().len(),
            1
        );
    }
}

#[tokio::test(start_paused = true)]
async fn background_routing_deadline_preserves_quota_and_does_not_fall_back() {
    let setup = setup_with_quota_backend(
        ApiKeyPermission::default(),
        policy("openai/gpt-5-codex", &["openai/gpt-4o"]),
        None,
        Some(("concurrent_requests", None)),
        Arc::new(MockBackend {
            submission_pending: true,
            ..MockBackend::default()
        }),
    )
    .await;
    let create = || {
        request(
            "/v1/responses",
            serde_json::json!({
                "model": "openai/gpt-5-codex",
                "input": "task",
                "background": true,
                "max_output_tokens": 32
            }),
        )
    };

    let response = setup.app.clone().oneshot(create()).await.unwrap();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    let body: serde_json::Value = serde_json::from_slice(&body_bytes(response).await).unwrap();
    assert_eq!(body["error"]["message"], "route attempt deadline exceeded");

    // A lost submission response does not prove that the provider stopped work.
    let blocked = setup.app.oneshot(create()).await.unwrap();
    assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(setup.backend.response_calls().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn foreground_generation_outlasting_the_routing_deadline_completes() {
    // The routing deadline only stops new fallback attempts; it must not cancel
    // a healthy foreground generation and drop its usage settlement.
    let setup = setup_with_quota_backend(
        ApiKeyPermission::default(),
        policy("openai/gpt-5-codex", &["openai/gpt-4o"]),
        None,
        Some(("concurrent_requests", None)),
        Arc::new(MockBackend {
            submission_delay: Some(std::time::Duration::from_secs(301)),
            ..MockBackend::default()
        }),
    )
    .await;
    let response = setup
        .app
        .oneshot(request(
            "/v1/responses",
            serde_json::json!({
                "model": "openai/gpt-5-codex",
                "input": "task",
                "max_output_tokens": 32
            }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(setup.backend.response_calls().len(), 1);
}

#[tokio::test]
async fn background_stream_disconnect_does_not_release_owned_quota() {
    let setup = setup_with_quota_backend(
        ApiKeyPermission::default(),
        FallbackPolicy::default(),
        Some(100_000),
        Some(("concurrent_requests", None)),
        Arc::new(MockBackend {
            background_queued: true,
            ..MockBackend::default()
        }),
    )
    .await;
    let create = || {
        request(
            "/v1/responses",
            serde_json::json!({
                "model":"openai/gpt-5-codex","input":"task","background":true,"stream":true,"max_output_tokens":32
            }),
        )
    };
    let accepted = setup.app.clone().oneshot(create()).await.unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);
    drop(accepted);
    let blocked = setup.app.clone().oneshot(create()).await.unwrap();
    assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(setup.backend.stream_calls().len(), 1);
    setup
        .state
        .background_responses
        .reconcile(&setup.state)
        .await
        .unwrap();
    assert_eq!(setup.backend.retrieve_calls(), ["resp_stream_integration"]);
    let admitted = setup.app.oneshot(create()).await.unwrap();
    assert_eq!(admitted.status(), StatusCode::OK);
}

#[tokio::test]
async fn background_explicit_rejection_can_fall_back_before_acceptance() {
    let setup = setup_with_quota(
        ApiKeyPermission::default(),
        policy(
            "openai/ratelimit",
            &["anthropic/claude-sonnet-5", "openai/gpt-5-codex"],
        ),
        Some(100_000),
        Some(("concurrent_requests", None)),
    )
    .await;
    let response=setup.app.oneshot(request("/v1/responses",serde_json::json!({"model":"openai/ratelimit","input":"task","background":true,"max_output_tokens":32}))).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-fallback-model"], "openai/gpt-5-codex");
    assert_eq!(setup.backend.response_calls().len(), 2);
    assert!(setup.backend.chat_calls().is_empty());
}

#[tokio::test]
async fn background_is_not_translated_to_chat_completions() {
    let setup = setup(ApiKeyPermission::default(), FallbackPolicy::default()).await;
    let response = setup
        .app
        .oneshot(request(
            "/v1/responses",
            serde_json::json!({
                "model": "anthropic/claude-sonnet-5",
                "input": "long task",
                "background": true
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(setup.backend.chat_calls().is_empty());
    assert!(setup.backend.response_calls().is_empty());
}

#[tokio::test]
async fn nonstream_resolves_primary_once_and_preserves_native_json() {
    let setup = setup(ApiKeyPermission::default(), FallbackPolicy::default()).await;
    let response = setup
        .app
        .oneshot(request(
            "/v1/responses",
            serde_json::json!({
                "model": "openai/gpt-5-codex",
                "input": "fix the bug",
                "stream": false,
                "include": ["reasoning.encrypted_content"],
                "client_metadata": {"future": true}
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-request-id"], "upstream-response-id");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes(response).await).unwrap();
    assert_eq!(body["status"], "completed");
    assert_eq!(body["codex_marker"], "unknown-response-field-preserved");
    assert!(body["id"].as_str().unwrap().starts_with("resp_chalk_1."));
    assert_eq!(setup.backend.resolve_calls.load(Ordering::SeqCst), 1);
    let calls = setup.backend.response_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]["include"],
        serde_json::json!(["reasoning.encrypted_content"])
    );
    assert_eq!(calls[0]["client_metadata"]["future"], true);
}

#[tokio::test]
async fn translated_anthropic_preserves_parallel_tool_calls() {
    for (stream, parallel_tool_calls) in [(false, false), (true, true)] {
        let setup = setup(ApiKeyPermission::default(), FallbackPolicy::default()).await;
        let response = setup
            .app
            .oneshot(request(
                "/v1/responses",
                serde_json::json!({
                    "model": "anthropic/claude-sonnet-5",
                    "input": "Hello",
                    "stream": stream,
                    "max_output_tokens": 64,
                    "parallel_tool_calls": parallel_tool_calls,
                    "tools": [{
                        "type": "function",
                        "name": "lookup",
                        "parameters": {"type": "object"}
                    }]
                }),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(body_bytes(response).await.to_vec()).unwrap();
        if stream {
            assert!(body.contains("event: response.completed"));
        } else {
            let body: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["parallel_tool_calls"], parallel_tool_calls);
        }
        let calls = setup.backend.chat_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].parallel_tool_calls, Some(parallel_tool_calls));
        assert_eq!(calls[0].max_completion_tokens, Some(64));
    }
}

#[tokio::test]
async fn translated_anthropic_without_output_cap_uses_adapter_default_for_quota() {
    for stream in [false, true] {
        let setup = setup_with_budget(
            ApiKeyPermission::default(),
            FallbackPolicy::default(),
            Some(5_000),
        )
        .await;
        let response = setup
            .app
            .oneshot(request(
                "/v1/responses",
                serde_json::json!({
                    "model": "anthropic/claude-sonnet-5",
                    "input": "Hello",
                    "stream": stream
                }),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(body_bytes(response).await.to_vec()).unwrap();
        if stream {
            assert!(body.contains("event: response.completed"));
        } else {
            let body: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["status"], "completed");
        }
        assert_eq!(setup.backend.chat_calls().len(), 1);
    }
}

#[tokio::test]
async fn translated_anthropic_accepts_and_omits_include() {
    for stream in [false, true] {
        let setup = setup(ApiKeyPermission::default(), FallbackPolicy::default()).await;
        let response = setup
            .app
            .oneshot(request(
                "/v1/responses",
                serde_json::json!({
                    "model": "anthropic/claude-sonnet-5",
                    "input": "Hello",
                    "stream": stream,
                    "include": ["reasoning.encrypted_content"]
                }),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(body_bytes(response).await.to_vec()).unwrap();
        if stream {
            assert!(body.contains("event: response.completed"));
        } else {
            let body: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["status"], "completed");
        }
        assert!(!body.contains("encrypted_content"));
        assert_eq!(setup.backend.chat_calls().len(), 1);
    }
}

#[tokio::test]
async fn translated_anthropic_rejects_unknown_include_selector() {
    let setup = setup(ApiKeyPermission::default(), FallbackPolicy::default()).await;
    let response = setup
        .app
        .oneshot(request(
            "/v1/responses",
            serde_json::json!({
                "model": "anthropic/claude-sonnet-5",
                "input": "Hello",
                "include": ["future.supplemental_field"]
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = String::from_utf8(body_bytes(response).await.to_vec()).unwrap();
    assert!(body.contains(
        "Responses field `include[0]` cannot be represented losslessly by chat completions"
    ));
    assert!(setup.backend.chat_calls().is_empty());
}

#[tokio::test]
async fn translated_rejection_names_the_unsupported_responses_field() {
    let setup = setup(ApiKeyPermission::default(), FallbackPolicy::default()).await;
    let response = setup
        .app
        .oneshot(request(
            "/v1/responses",
            serde_json::json!({
                "model": "anthropic/claude-sonnet-5",
                "input": "Hello",
                "reasoning": {"effort": "medium", "summary": "auto"}
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = String::from_utf8(body_bytes(response).await.to_vec()).unwrap();
    assert!(
        body.contains(
            "Responses field `reasoning.summary` cannot be represented losslessly by chat completions"
        ),
        "{body}"
    );
    assert!(setup.backend.chat_calls().is_empty());
    assert!(setup.backend.response_calls().is_empty());
}

#[tokio::test]
async fn translated_reasoning_effort_reaches_chat_as_reasoning_effort() {
    let setup = setup(ApiKeyPermission::default(), FallbackPolicy::default()).await;
    let response = setup
        .app
        .oneshot(request(
            "/v1/responses",
            serde_json::json!({
                "model": "anthropic/claude-sonnet-5",
                "input": "Hello",
                "reasoning": {"effort": "medium"}
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let calls = setup.backend.chat_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].reasoning_effort.as_deref(), Some("medium"));
    assert!(setup.backend.response_calls().is_empty());
}

#[tokio::test]
async fn nonstream_bad_upstream_model_is_terminal() {
    let primary = "openai/bad-upstream-model";
    let fallback = "openai/gpt-5-codex";
    let setup = setup(ApiKeyPermission::default(), policy(primary, &[fallback])).await;
    let response = setup
        .app
        .oneshot(request(
            "/v1/responses",
            serde_json::json!({"model": primary, "input": "fix the bug"}),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(response.headers().get("x-fallback-model").is_none());
    let calls = setup.backend.response_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["model"], primary);
}

#[tokio::test]
async fn opaque_response_ids_route_continuation_retrieve_and_delete() {
    let setup = setup(ApiKeyPermission::default(), FallbackPolicy::default()).await;
    let created = setup
        .app
        .clone()
        .oneshot(request(
            "/v1/responses",
            serde_json::json!({"model": "openai/gpt-5-codex", "input": "first"}),
        ))
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body_bytes(created).await).unwrap();
    let response_id = created["id"].as_str().unwrap();
    assert!(response_id.starts_with("resp_chalk_1."));

    let continued = setup
        .app
        .clone()
        .oneshot(request(
            "/v1/responses",
            serde_json::json!({
                "model": "openai/gpt-5-codex",
                "input": "continue",
                "previous_response_id": response_id
            }),
        ))
        .await
        .unwrap();
    assert_eq!(continued.status(), StatusCode::OK);
    assert_eq!(
        setup.backend.response_calls().last().unwrap()["previous_response_id"],
        "resp_integration"
    );

    let retrieved = setup
        .app
        .clone()
        .oneshot(response_operation_request("GET", response_id))
        .await
        .unwrap();
    assert_eq!(retrieved.status(), StatusCode::OK);
    let retrieved: serde_json::Value =
        serde_json::from_slice(&body_bytes(retrieved).await).unwrap();
    assert_eq!(retrieved["id"], response_id);
    assert_eq!(setup.backend.retrieve_calls(), vec!["resp_integration"]);

    let resumed = setup
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/v1/responses/{response_id}?stream=true&starting_after=42"
                ))
                .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resumed.status(), StatusCode::OK);
    assert_eq!(resumed.headers()[header::CONTENT_TYPE], "text/event-stream");
    assert_eq!(resumed.headers()["x-request-id"], "resume-request-id");
    let resumed = String::from_utf8(body_bytes(resumed).await.to_vec()).unwrap();
    assert!(resumed.contains("\"sequence_number\":43"));
    assert!(resumed.contains("\"sequence_number\":44"));
    assert!(resumed.contains(response_id));
    assert!(!resumed.contains("\"response_id\":\"resp_integration\""));
    assert_eq!(
        setup.backend.retrieve_stream_calls(),
        vec![("resp_integration".to_string(), Some(42))]
    );

    let cancelled = setup
        .app
        .clone()
        .oneshot(response_operation_request(
            "POST",
            &format!("{response_id}/cancel"),
        ))
        .await
        .unwrap();
    assert_eq!(cancelled.status(), StatusCode::OK);
    let cancelled: serde_json::Value =
        serde_json::from_slice(&body_bytes(cancelled).await).unwrap();
    assert_eq!(cancelled["id"], response_id);
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(setup.backend.cancel_calls(), vec!["resp_integration"]);

    let deleted = setup
        .app
        .oneshot(response_operation_request("DELETE", response_id))
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::OK);
    let deleted: serde_json::Value = serde_json::from_slice(&body_bytes(deleted).await).unwrap();
    assert_eq!(deleted["id"], response_id);
    assert_eq!(deleted["deleted"], true);
    assert_eq!(setup.backend.delete_calls(), vec!["resp_integration"]);
}

#[tokio::test]
async fn starting_after_requires_streaming_retrieval() {
    let setup = setup(ApiKeyPermission::default(), FallbackPolicy::default()).await;
    let locator =
        openai_server::response_locator::ResponseLocatorCodec::new(b"test-locator".to_vec())
            .encode("connection-1", None, "resp_integration");
    let response = setup
        .app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/responses/{locator}?starting_after=42"))
                .header(header::AUTHORIZATION, format!("Bearer {TEST_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(setup.backend.retrieve_calls().is_empty());
    assert!(setup.backend.retrieve_stream_calls().is_empty());
}

/// A response created through a model route carries the route in its id, so a
/// key granted only that route can retrieve it, and nothing else.
#[tokio::test]
async fn route_grant_retrieves_responses_created_through_the_route() {
    let permission = ApiKeyPermission {
        route_ids: Some(vec!["route-codex".to_string()]),
        ..Default::default()
    };
    let setup = setup(permission, FallbackPolicy::default()).await;
    let created = setup
        .app
        .clone()
        .oneshot(request(
            "/v1/responses",
            serde_json::json!({"model": "chalk/codex", "input": "first"}),
        ))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::OK);
    let created: serde_json::Value = serde_json::from_slice(&body_bytes(created).await).unwrap();
    let response_id = created["id"].as_str().unwrap().to_string();
    let codec =
        openai_server::response_locator::ResponseLocatorCodec::new(b"test-locator".to_vec());
    assert_eq!(
        codec
            .decode(&response_id)
            .unwrap()
            .model_route_id
            .as_deref(),
        Some("route-codex")
    );

    let retrieved = setup
        .app
        .clone()
        .oneshot(response_operation_request("GET", &response_id))
        .await
        .unwrap();
    assert_eq!(retrieved.status(), StatusCode::OK);
    let retrieved: serde_json::Value =
        serde_json::from_slice(&body_bytes(retrieved).await).unwrap();
    assert_eq!(retrieved["id"], response_id);

    for locator in [
        codec.encode("connection-1", None, "resp_integration"),
        codec.encode("connection-1", Some("route-other"), "resp_integration"),
    ] {
        let response = setup
            .app
            .clone()
            .oneshot(response_operation_request("GET", &locator))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    assert_eq!(setup.backend.retrieve_calls().len(), 1);

    // The direct model on the same connection is not granted.
    let direct = setup
        .app
        .clone()
        .oneshot(request(
            "/v1/responses",
            serde_json::json!({"model": "openai/gpt-5-codex", "input": "hi"}),
        ))
        .await
        .unwrap();
    assert_eq!(direct.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn primary_permission_denial_dispatches_zero_times() {
    let permission = ApiKeyPermission {
        provider: Some("anthropic".to_string()),
        ..Default::default()
    };
    let setup = setup(permission, FallbackPolicy::default()).await;
    let response = setup
        .app
        .oneshot(request(
            "/v1/responses",
            serde_json::json!({"model": "openai/gpt-5-codex", "input": "hi"}),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(setup.backend.response_calls().is_empty());
    assert!(setup.backend.stream_calls().is_empty());
    assert_eq!(setup.backend.resolve_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn incompatible_fallback_preserves_primary_rate_limit_and_retry_after() {
    let primary = "openai/ratelimit-primary";
    let setup = setup(
        ApiKeyPermission::default(),
        policy(primary, &["compatible/incompatible-fallback"]),
    )
    .await;
    let response = setup
        .app
        .oneshot(request(
            "/v1/responses",
            serde_json::json!({
                "model": primary,
                "input": "hi",
                "reasoning": {"effort": "medium"}
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()[header::RETRY_AFTER], "17");
    let calls = setup.backend.response_calls();
    assert_eq!(calls.len(), 1, "incompatible fallback must not dispatch");
    assert_eq!(calls[0]["model"], primary);
}

#[tokio::test]
async fn streaming_response_keeps_named_sse_terminal_and_native_body() {
    let setup = setup(ApiKeyPermission::default(), FallbackPolicy::default()).await;
    let response = setup
        .app
        .oneshot(request(
            "/v1/responses",
            serde_json::json!({
                "model": "openai/gpt-5-codex",
                "input": "hi",
                "stream": true
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    assert_eq!(response.headers()["x-request-id"], "upstream-stream-id");
    let body = String::from_utf8(body_bytes(response).await.to_vec()).unwrap();
    assert!(body.contains("event: response.created\r\n"));
    assert!(body.contains("event: response.completed\n"));
    assert!(body.contains("served-by:openai/gpt-5-codex"));
    assert!(body.contains("unknown-response-field-preserved"));
    assert!(body.contains("resp_chalk_1."));
    assert_eq!(setup.backend.stream_calls().len(), 1);
    assert!(setup.backend.response_calls().is_empty());
}

#[tokio::test]
async fn stream_open_bad_upstream_model_is_terminal() {
    let primary = "openai/bad-upstream-model";
    let fallback = "openai/gpt-5-codex";
    let setup = setup(ApiKeyPermission::default(), policy(primary, &[fallback])).await;
    let response = setup
        .app
        .oneshot(request(
            "/v1/responses",
            serde_json::json!({"model": primary, "input": "fix the bug", "stream": true}),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(response.headers().get("x-fallback-model").is_none());
    let calls = setup.backend.stream_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["model"], primary);
}

#[tokio::test]
async fn compact_dispatches_natively_and_preserves_opaque_item() {
    let setup = setup(ApiKeyPermission::default(), FallbackPolicy::default()).await;
    let response = setup
        .app
        .oneshot(request(
            "/v1/responses/compact",
            serde_json::json!({
                "model": "openai/gpt-5-codex",
                "instructions": "continue",
                "input": [{"type": "message", "role": "user", "content": "hello"}]
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(&body_bytes(response).await).unwrap();
    assert_eq!(body["object"], "response.compaction");
    assert_eq!(body["output"][0]["type"], "compaction");
    assert_eq!(
        body["output"][0]["encrypted_content"],
        "opaque-compaction-item"
    );
    let calls = setup.backend.compact_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["input"][0]["role"], "user");
    assert_eq!(setup.backend.resolve_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn compact_preparation_success_and_upstream_failure_settle_quota() {
    let (setup, quota) =
        setup_with_recording_quota(ApiKeyPermission::default(), FallbackPolicy::default()).await;
    for body in [
        serde_json::json!([]),
        serde_json::json!({"model":null,"input":"hello"}),
    ] {
        let response = setup
            .app
            .clone()
            .oneshot(request("/v1/responses/compact", body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    assert_eq!(quota.acquisitions.load(Ordering::SeqCst), 0);
    assert!(setup.backend.compact_calls().is_empty());
    let response = setup
        .app
        .clone()
        .oneshot(request(
            "/v1/responses/compact",
            serde_json::json!({"model":"openai/gpt-5-codex","input":"hello"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    body_bytes(response).await;
    assert_eq!(quota.acquisitions.load(Ordering::SeqCst), 1);
    assert_eq!(quota.finishes.load(Ordering::SeqCst), 1);
    assert_eq!(setup.backend.compact_calls().len(), 1);
    let response = setup.app.oneshot(request("/v1/responses/compact", serde_json::json!({"model":"openai/gpt-5-codex","input":"hello","simulate_failure":true}))).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(quota.acquisitions.load(Ordering::SeqCst), 2);
    assert_eq!(quota.finishes.load(Ordering::SeqCst), 2);
    assert_eq!(setup.backend.compact_calls().len(), 2);
}

#[tokio::test]
async fn compact_is_rejected_by_the_token_budget_before_dispatch() {
    let setup = setup_with_budget(
        ApiKeyPermission::default(),
        FallbackPolicy::default(),
        Some(1),
    )
    .await;
    let response = setup
        .app
        .oneshot(request(
            "/v1/responses/compact",
            serde_json::json!({
                "model": "openai/gpt-5-codex",
                "instructions": "continue",
                "input": [{"type": "message", "role": "user", "content": "hello"}]
            }),
        ))
        .await
        .unwrap();

    // Compaction has no output bound here, so a matching budget cannot admit it.
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(setup.backend.compact_calls().is_empty());
}

#[tokio::test]
async fn nonstream_settles_quota_exactly_once_through_shared_settlement() {
    let (setup, finishes) =
        setup_with_recording_quota(ApiKeyPermission::default(), FallbackPolicy::default()).await;

    let response = setup
        .app
        .oneshot(request(
            "/v1/responses",
            serde_json::json!({
                "model": "openai/gpt-5-codex",
                "input": "fix the bug",
                "stream": false
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(&body_bytes(response).await).unwrap();
    assert_eq!(body["usage"]["total_tokens"], 5);

    // The reserved concurrency permit must be reconciled exactly once by
    // `usage::with_quota_settlement`: never left dangling (0) and never
    // settled twice by both the reservation fold and the usage write path.
    assert_eq!(finishes.finishes.load(Ordering::SeqCst), 1);
}
