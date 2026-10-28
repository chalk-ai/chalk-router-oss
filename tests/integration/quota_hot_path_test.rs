//! Deterministic lock-order races and cache freshness/accounting, using actual
//! admission decisions rather than HTTP status (backend errors can fail open).
use chrono::Utc;
use openai_server::{
    api_keys::{ApiKeyStore, InMemoryApiKeyStore, PostgresApiKeyStore},
    budget::{self, BudgetPolicyStore},
    hashing::KeyHasher,
    quota::{
        MemoryQuota, PostgresQuota, QuotaBackend, QuotaDecision, QuotaOutcome, TokenReservation,
    },
    quota_policy::QuotaPolicyResolver,
    ratelimit::{self, ApiKeyRateLimitInput, RateLimitMatch, RateLimitModel, RateLimitPolicyStore},
    service::api_keys::{ApiKeyService, CreateKeyInput, UpdateKeyInput},
};
use std::sync::Arc;

async fn pool() -> Option<sqlx::PgPool> {
    let url = crate::support::db::test_database_url("quota_hot_path")?;
    openai_server::migrate::run(&url).await.unwrap();
    Some(
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect_with(openai_server::database::connect_options(&url).unwrap())
            .await
            .unwrap(),
    )
}

async fn cached_atomic_suite(pool: Option<sqlx::PgPool>) {
    let env = format!("quota-cache-{}", uuid::Uuid::new_v4());
    let hasher = KeyHasher::new(b"cache-test".to_vec());
    let keys: Arc<dyn ApiKeyStore> = match &pool {
        Some(pool) => Arc::new(PostgresApiKeyStore::new(pool.clone(), hasher, env.clone())),
        None => Arc::new(InMemoryApiKeyStore::new(hasher)),
    };
    let budgets = budget::from_core_store(pool.clone(), env.clone());
    let rates = ratelimit::from_core_store(pool.clone(), env.clone());
    let service = ApiKeyService::new(
        keys,
        None,
        budgets.clone(),
        rates.clone(),
        std::sync::Arc::new(
            openai_server::provider_connections::InMemoryProviderConnectionStore::new(),
        ),
        std::sync::Arc::new(openai_server::model_routes::InMemoryModelRouteStore::new()),
        true,
    );
    let created = service
        .create(CreateKeyInput {
            description: Some("cache test".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let key = &created.api_key.key_hash;
    let resolver = QuotaPolicyResolver::new(env.clone(), budgets.clone(), rates.clone());
    let matching = RateLimitMatch {
        key_hash: key,
        usage_pool_id: None,
        connection_id: None,
        model: RateLimitModel::NotApplicable,
    };
    assert!(budgets.matching_shared(key, None).await.unwrap().is_empty());
    assert!(rates.matching_shared(&matching).await.unwrap().is_empty());
    service
        .update(
            key,
            UpdateKeyInput {
                daily_token_budget: Some(Some(10)),
                rate_limits: Some(vec![ApiKeyRateLimitInput {
                    requests_per_minute: Some(1),
                    request_burst: Some(1),
                    enabled: true,
                    connection_id: None,
                    models: vec![],
                    tokens_per_minute: None,
                    token_burst: None,
                    concurrent_requests: None,
                }]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(budgets.matching_shared(key, None).await.unwrap().len(), 1);
    assert_eq!(rates.matching_shared(&matching).await.unwrap().len(), 1);
    let plan = resolver
        .request_actions(key, None, TokenReservation::Known(10), Utc::now())
        .await
        .unwrap();
    assert_eq!(plan.actions.len(), 2);
    let backend: Arc<dyn QuotaBackend> = match &pool {
        Some(pool) => Arc::new(PostgresQuota::new(pool.clone())),
        None => Arc::new(MemoryQuota::new()),
    };
    let QuotaDecision::Allowed(Some(grant)) =
        backend.acquire_many(&plan.actions, None).await.unwrap()
    else {
        panic!("first request allowed")
    };
    backend
        .finish(&grant, QuotaOutcome::Actual(10))
        .await
        .unwrap();
    assert!(matches!(
        backend.acquire_many(&plan.actions, None).await.unwrap(),
        QuotaDecision::Limited(_)
    ));
    service
        .update(
            key,
            UpdateKeyInput {
                daily_token_budget: Some(None),
                rate_limits: Some(vec![]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(budgets.matching_shared(key, None).await.unwrap().is_empty());
    assert!(rates.matching_shared(&matching).await.unwrap().is_empty());
    service.revoke(key).await.unwrap();
}

#[tokio::test]
async fn memory_cached_absence_atomic_edits_and_denials() {
    cached_atomic_suite(None).await;
}
#[tokio::test]
async fn postgres_cached_absence_atomic_edits_and_denials() {
    if let Some(pool) = pool().await {
        cached_atomic_suite(Some(pool)).await;
    }
}

async fn replica_expiry_suite(pool: Option<sqlx::PgPool>) {
    use openai_server::quota_definition_cache::{
        CachedBudgetPolicyStore, CachedRateLimitPolicyStore,
    };
    let env = format!("quota-replica-{}", uuid::Uuid::new_v4());
    let (raw_budget, raw_rate): (Arc<dyn BudgetPolicyStore>, Arc<dyn RateLimitPolicyStore>) =
        match pool {
            Some(pool) => (
                Arc::new(budget::PostgresBudgetPolicyStore::new(
                    pool.clone(),
                    env.clone(),
                )),
                Arc::new(ratelimit::PostgresRateLimitPolicyStore::new(pool, env)),
            ),
            None => (
                Arc::new(budget::MemoryBudgetPolicyStore::new(env.clone())),
                Arc::new(ratelimit::MemoryRateLimitPolicyStore::new(env)),
            ),
        };
    let writer = CachedBudgetPolicyStore::new(raw_budget.clone());
    let reader = CachedBudgetPolicyStore::new(raw_budget);
    let rate_writer = CachedRateLimitPolicyStore::new(raw_rate.clone());
    let rate_reader = CachedRateLimitPolicyStore::new(raw_rate);
    let request = RateLimitMatch {
        key_hash: "key",
        usage_pool_id: None,
        connection_id: None,
        model: RateLimitModel::NotApplicable,
    };
    assert!(reader
        .matching_shared("key", None)
        .await
        .unwrap()
        .is_empty());
    assert!(rate_reader
        .matching_shared(&request)
        .await
        .unwrap()
        .is_empty());
    writer
        .replace_key_budgets(
            "key",
            vec![budget::ApiKeyBudgetInput {
                token_limit: 10,
                period: "daily".into(),
                enabled: true,
            }],
        )
        .await
        .unwrap();
    rate_writer
        .create(ratelimit::RateLimitPolicy {
            quota_policy_id: String::new(),
            id: uuid::Uuid::new_v4().to_string(),
            limit_type: "requests_per_minute".into(),
            limit_value: 1,
            burst_value: Some(1),
            connection_id: None,
            models: vec![],
            scope: "api_key".into(),
            scope_id: "key".into(),
            enabled: true,
            created_at: Utc::now(),
        })
        .await
        .unwrap();
    assert_eq!(writer.matching_shared("key", None).await.unwrap().len(), 1);
    assert_eq!(
        rate_writer.matching_shared(&request).await.unwrap().len(),
        1
    );
    assert!(reader
        .matching_shared("key", None)
        .await
        .unwrap()
        .is_empty());
    tokio::time::sleep(std::time::Duration::from_millis(1050)).await;
    assert_eq!(reader.matching_shared("key", None).await.unwrap().len(), 1);
    assert_eq!(
        rate_reader.matching_shared(&request).await.unwrap().len(),
        1
    );
    assert!(reader
        .matching_shared("unrelated", None)
        .await
        .unwrap()
        .is_empty());
}
#[tokio::test]
async fn memory_two_replicas_refresh_cached_absence() {
    replica_expiry_suite(None).await;
}
#[tokio::test]
async fn postgres_two_replicas_refresh_cached_absence() {
    if let Some(pool) = pool().await {
        replica_expiry_suite(Some(pool)).await;
    }
}

async fn cached_pool_suite(pool: Option<sqlx::PgPool>) {
    use openai_server::service::usage_pools::{
        CreateUsagePoolInput, UpdateUsagePoolInput, UsagePoolService,
    };
    use openai_server::usage_pools::{
        InMemoryUsagePoolStore, PostgresUsagePoolStore, UsagePoolStore,
    };
    let env = format!("pool-cache-{}", uuid::Uuid::new_v4());
    let pools: Arc<dyn UsagePoolStore> = match &pool {
        Some(pool) => Arc::new(PostgresUsagePoolStore::new(pool.clone(), env.clone())),
        None => Arc::new(InMemoryUsagePoolStore::new()),
    };
    let budgets = budget::from_core_store(pool.clone(), env.clone());
    let rates = ratelimit::from_core_store(pool, env.clone());
    let service = UsagePoolService::new(pools, budgets.clone(), rates.clone(), env, true);
    let created = service
        .create(CreateUsagePoolInput {
            name: "cache test".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let matching = RateLimitMatch {
        key_hash: "key",
        usage_pool_id: Some(&created.id),
        connection_id: None,
        model: RateLimitModel::NotApplicable,
    };
    assert!(budgets
        .matching_shared("key", Some(&created.id))
        .await
        .unwrap()
        .is_empty());
    assert!(rates.matching_shared(&matching).await.unwrap().is_empty());
    service
        .update(
            &created.id,
            UpdateUsagePoolInput {
                usage_budgets: Some(vec![budget::ApiKeyBudgetInput {
                    token_limit: 10,
                    period: "daily".into(),
                    enabled: true,
                }]),
                rate_limits: Some(vec![ApiKeyRateLimitInput {
                    connection_id: None,
                    models: vec![],
                    requests_per_minute: Some(1),
                    request_burst: Some(1),
                    tokens_per_minute: None,
                    token_burst: None,
                    concurrent_requests: None,
                    enabled: true,
                }]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        budgets
            .matching_shared("key", Some(&created.id))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(rates.matching_shared(&matching).await.unwrap().len(), 1);
    assert!(budgets
        .matching_shared("key", None)
        .await
        .unwrap()
        .is_empty());
    service.delete(&created.id).await.unwrap();
    assert!(budgets
        .matching_shared("key", Some(&created.id))
        .await
        .unwrap()
        .is_empty());
    assert!(rates.matching_shared(&matching).await.unwrap().is_empty());
}
#[tokio::test]
async fn memory_cached_pool_atomic_writes_and_deletion() {
    cached_pool_suite(None).await;
}
#[tokio::test]
async fn postgres_cached_pool_atomic_writes_and_deletion() {
    if let Some(pool) = pool().await {
        cached_pool_suite(Some(pool)).await;
    }
}

struct TerminalFixture {
    pool: sqlx::PgPool,
    env: String,
    policies: Vec<String>,
    backend: Arc<dyn QuotaBackend>,
    grants: Vec<openai_server::quota::QuotaGrant>,
}

impl TerminalFixture {
    async fn new(pool: sqlx::PgPool) -> Self {
        Self::with_policies(pool, 2).await
    }

    async fn with_policies(pool: sqlx::PgPool, count: usize) -> Self {
        use openai_server::quota::{QuotaAction, QuotaAlgorithm, QuotaMeter, QuotaSource};
        let env = format!("terminal-{}", uuid::Uuid::new_v4());
        let backend: Arc<dyn QuotaBackend> = Arc::new(PostgresQuota::new(pool.clone()));
        let start = Utc::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let mut policies = Vec::new();
        let mut grants = Vec::new();
        for index in 0..count {
            let id = format!("{env}:{index}");
            sqlx::query("WITH b AS (INSERT INTO chalk_router.budget_limits(environment_id,id,scope,scope_id,window_type,window_size,limit_kind,limit_amount,enabled) VALUES($1,$2,'api_key',$2,'calendar',INTERVAL '1 day','tokens',1000,true)) INSERT INTO chalk_router.quota_policies(id,environment_id,budget_limit_id) VALUES($2,$1,$2)")
                .bind(&env).bind(&id).execute(&pool).await.unwrap();
            let action = QuotaAction {
                environment_id: env.clone(),
                policy_id: id.clone(),
                source: QuotaSource::BudgetLimit,
                algorithm: QuotaAlgorithm::FixedWindow,
                meter: QuotaMeter::Tokens,
                limit_value: 1000,
                burst: None,
                amount: 10,
                window_start: Some(start),
                window_end: Some(start + chrono::Duration::days(1)),
            };
            let QuotaDecision::Allowed(Some(grant)) =
                backend.acquire_many(&[action], None).await.unwrap()
            else {
                panic!("admit terminal fixture")
            };
            policies.push(id);
            grants.push(grant);
        }
        Self {
            pool,
            env,
            policies,
            backend,
            grants,
        }
    }

    fn permit(&self) -> openai_server::quota::QuotaPermit {
        use openai_server::quota::QuotaPermit;
        // Deliberately reverse grant order: the function must lock their union.
        QuotaPermit::merge(
            Some(QuotaPermit::new(
                self.backend.clone(),
                self.grants[1].clone(),
            )),
            Some(QuotaPermit::new(
                self.backend.clone(),
                self.grants[0].clone(),
            )),
        )
        .unwrap()
    }

    fn record(&self, event_id: uuid::Uuid) -> openai_server::usage_tracker::UsageRecord<'_> {
        openai_server::usage_tracker::UsageRecord {
            event_id,
            time: Utc::now(),
            key_hash: "terminal-key",
            usage_pool_id: None,
            connection_id: "terminal-connection",
            provider_kind: "openai",
            model: "terminal-model",
            prompt_tokens: 2,
            completion_tokens: 2,
            cached_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            usage_source: openai_server::usage_tracker::UsageSource::Reported,
            input_token_attribution: Default::default(),
        }
    }

    async fn balances(&self) -> Vec<i64> {
        sqlx::query_scalar("SELECT used_amount FROM chalk_router.quota_states WHERE policy_id=ANY($1) ORDER BY policy_id")
            .bind(&self.policies).fetch_all(&self.pool).await.unwrap()
    }

    async fn finish_sql(&self, source: i16) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT chalk_router.finish_quota_usage($1,4,$2,now(),$3,'terminal-key',NULL,'terminal-connection','terminal-model',2,2,0,0,0,4,$4,NULL)")
            .bind(sqlx::types::Json(&self.grants)).bind(uuid::Uuid::new_v4()).bind(&self.env)
            .bind(source).execute(&self.pool).await.map(|_| ())
    }
}

#[tokio::test]
async fn postgres_terminal_accounting_replays_receipts_with_existing_usage() {
    use openai_server::usage_tracker::UsageTracker;
    let Some(pool) = pool().await else { return };
    let fixture = TerminalFixture::new(pool.clone()).await;
    let tracker = UsageTracker::new(pool, fixture.env.clone()).await.unwrap();
    let event = uuid::Uuid::new_v4();
    // A standalone usage row must not prevent settlement of outstanding grants.
    tracker.record(fixture.record(event)).await.unwrap();
    for _ in 0..2 {
        tracker
            .record_with_quota(fixture.record(event), Some(fixture.permit()), Some(4))
            .await
            .unwrap();
        assert_eq!(fixture.balances().await, vec![4, 4]);
    }
    let total: (i64, i64) = sqlx::query_as("SELECT COUNT(*)::BIGINT, SUM(total_tokens)::BIGINT FROM chalk_router.usage_events WHERE environment_id=$1")
        .bind(&fixture.env).fetch_one(&fixture.pool).await.unwrap();
    assert_eq!(total, (1, 4));
}

#[tokio::test]
async fn postgres_gemini_modality_cost_settles_inclusive_tokens_once() {
    use openai_server::{pricing::InputTokenAttribution, usage_tracker::UsageTracker};
    let Some(pool) = pool().await else { return };
    for (attribution, expected_cost) in [
        (
            InputTokenAttribution::TextAndImage {
                text_tokens: 7,
                image_tokens: 258,
            },
            Some(0.0001175),
        ),
        (InputTokenAttribution::Unknown, None),
    ] {
        let fixture = TerminalFixture::new(pool.clone()).await;
        let tracker = UsageTracker::new(pool.clone(), fixture.env.clone())
            .await
            .unwrap();
        let event = uuid::Uuid::new_v4();
        for _ in 0..2 {
            let mut record = fixture.record(event);
            record.provider_kind = "gemini";
            record.model = "gemini-embedding-2";
            record.prompt_tokens = 265;
            record.completion_tokens = 0;
            record.input_token_attribution = attribution;
            tracker
                .record_with_quota(record, Some(fixture.permit()), Some(265))
                .await
                .unwrap();
            assert_eq!(fixture.balances().await, vec![265, 265]);
        }
        let result: (i64, i64, Option<f64>) = sqlx::query_as(
            "SELECT COUNT(*)::BIGINT, SUM(total_tokens)::BIGINT, SUM(estimated_cost_usd) \
             FROM chalk_router.usage_events WHERE environment_id=$1",
        )
        .bind(&fixture.env)
        .fetch_one(&fixture.pool)
        .await
        .unwrap();
        assert_eq!((result.0, result.1), (1, 265));
        match expected_cost {
            Some(expected) => assert!((result.2.unwrap() - expected).abs() < 1e-12),
            None => assert_eq!(result.2, None),
        }
    }
}

#[tokio::test]
async fn postgres_terminal_usage_failure_rolls_back_all_settlements() {
    let Some(pool) = pool().await else { return };
    let fixture = TerminalFixture::new(pool).await;
    assert!(
        fixture.finish_sql(0).await.is_err(),
        "invalid usage provenance rejected"
    );
    assert_eq!(fixture.balances().await, vec![10, 10]);
    let outstanding: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chalk_router.quota_reservations WHERE permit_id=ANY($1)",
    )
    .bind(
        fixture
            .grants
            .iter()
            .map(|grant| grant.permit_id)
            .collect::<Vec<_>>(),
    )
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(outstanding, 2);
    fixture.finish_sql(1).await.unwrap();
    assert_eq!(fixture.balances().await, vec![4, 4]);
}

#[tokio::test]
async fn postgres_terminal_accounting_prelocks_union_across_grants() {
    let Some(pool) = pool().await else { return };
    let fixture = Arc::new(TerminalFixture::new(pool.clone()).await);
    let mut holder = pool.begin().await.unwrap();
    let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    sqlx::query("SELECT policy_id FROM chalk_router.quota_states WHERE policy_id=$1 FOR UPDATE")
        .bind(&fixture.policies[0])
        .execute(&mut *holder)
        .await
        .unwrap();
    let worker_fixture = fixture.clone();
    let task = tokio::spawn(async move {
        let tracker = openai_server::usage_tracker::UsageTracker::new(
            worker_fixture.pool.clone(),
            worker_fixture.env.clone(),
        )
        .await
        .unwrap();
        tracker
            .record_with_quota(
                worker_fixture.record(uuid::Uuid::new_v4()),
                Some(worker_fixture.permit()),
                Some(4),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE query LIKE 'SELECT chalk_router.finish_quota_usage%' AND $1=ANY(pg_blocking_pids(pid)))")
                .bind(holder_pid).fetch_one(&pool).await.unwrap();
            if blocked { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("combined settlement waits on lower policy first");
    // If the worker settled the higher grant first, this would deadlock.
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        sqlx::query(
            "SELECT policy_id FROM chalk_router.quota_states WHERE policy_id=$1 FOR UPDATE",
        )
        .bind(&fixture.policies[1])
        .execute(&mut *holder),
    )
    .await
    .unwrap()
    .unwrap();
    holder.commit().await.unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(fixture.balances().await, vec![4, 4]);
}

#[tokio::test]
async fn postgres_terminal_accounting_cancellation_keeps_cleanup_ownership() {
    let Some(pool) = pool().await else { return };
    let fixture = Arc::new(TerminalFixture::new(pool.clone()).await);
    let mut holder = pool.begin().await.unwrap();
    let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    sqlx::query("SELECT policy_id FROM chalk_router.quota_states WHERE policy_id=$1 FOR UPDATE")
        .bind(&fixture.policies[0])
        .execute(&mut *holder)
        .await
        .unwrap();
    let worker_fixture = fixture.clone();
    let task = tokio::spawn(async move {
        let tracker = openai_server::usage_tracker::UsageTracker::new(
            worker_fixture.pool.clone(),
            worker_fixture.env.clone(),
        )
        .await
        .unwrap();
        tracker
            .record_with_quota(
                worker_fixture.record(uuid::Uuid::new_v4()),
                Some(worker_fixture.permit()),
                Some(4),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE query LIKE 'SELECT chalk_router.finish_quota_usage%' AND $1=ANY(pg_blocking_pids(pid)))")
                .bind(holder_pid).fetch_one(&pool).await.unwrap();
            if blocked { break; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    holder.rollback().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let remaining: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM chalk_router.quota_reservations WHERE permit_id=ANY($1)",
            )
            .bind(
                fixture
                    .grants
                    .iter()
                    .map(|grant| grant.permit_id)
                    .collect::<Vec<_>>(),
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if remaining == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Drop cleans up both owned grants");
    let balances = fixture.balances().await;
    assert!(
        balances == vec![10, 10] || balances == vec![4, 4],
        "cancellation either precedes or follows atomic commit: {balances:?}"
    );
}

#[tokio::test]
async fn postgres_terminal_quota_failure_still_records_durable_usage() {
    terminal_quota_failure_still_records_durable_usage("P0001", 2).await;
}

#[tokio::test]
async fn postgres_transient_terminal_failure_reserves_standalone_usage_attempt() {
    terminal_quota_failure_still_records_durable_usage("40001", 5).await;
}

async fn terminal_quota_failure_still_records_durable_usage(
    sqlstate: &str,
    expected_attempts: i64,
) {
    assert!(matches!(sqlstate, "P0001" | "40001"));
    let Some(pool) = pool().await else { return };
    let fixture = TerminalFixture::new(pool.clone()).await;
    // Sequences survive rollback, unlike an ordinary counter row.
    sqlx::raw_sql("CREATE SEQUENCE chalk_router.test_terminal_failure_attempts")
        .execute(&pool)
        .await
        .unwrap();
    // Reject only this fixture's settlement updates, after admission completed.
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE FUNCTION chalk_router.test_reject_terminal_settlement() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.policy_id = TG_ARGV[0] AND NEW.used_amount < OLD.used_amount THEN PERFORM nextval('chalk_router.test_terminal_failure_attempts'); RAISE EXCEPTION 'injected quota settlement failure' USING ERRCODE='{sqlstate}'; END IF; RETURN NEW; END $$")))
        .execute(&pool).await.unwrap();
    let statement = format!("CREATE TRIGGER test_reject_terminal_settlement BEFORE UPDATE ON chalk_router.quota_states FOR EACH ROW EXECUTE FUNCTION chalk_router.test_reject_terminal_settlement('{}')", fixture.policies[0]);
    sqlx::query(sqlx::AssertSqlSafe(statement))
        .execute(&pool)
        .await
        .unwrap();
    let tracker =
        openai_server::usage_tracker::UsageTracker::new(pool.clone(), fixture.env.clone())
            .await
            .unwrap();
    let result = tracker
        .record_with_quota(
            fixture.record(uuid::Uuid::new_v4()),
            Some(fixture.permit()),
            Some(4),
        )
        .await;
    let attempts: i64 =
        sqlx::query_scalar("SELECT last_value FROM chalk_router.test_terminal_failure_attempts")
            .fetch_one(&pool)
            .await
            .unwrap();
    sqlx::raw_sql("DROP TRIGGER test_reject_terminal_settlement ON chalk_router.quota_states; DROP FUNCTION chalk_router.test_reject_terminal_settlement(); DROP SEQUENCE chalk_router.test_terminal_failure_attempts")
        .execute(&pool).await.unwrap();
    result.unwrap();
    assert_eq!(
        attempts, expected_attempts,
        "combined attempts plus one separate failed settlement"
    );
    let usage: i64 = sqlx::query_scalar(
        "SELECT SUM(total_tokens)::BIGINT FROM chalk_router.usage_events WHERE environment_id=$1",
    )
    .bind(&fixture.env)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        usage, 4,
        "settlement errors do not suppress successful durable usage"
    );
    assert_eq!(fixture.balances().await, vec![10, 4]);
    // Failed settlement remains bounded by reservation expiry; clean the test
    // receipt explicitly once the injected failure is removed.
    fixture
        .backend
        .finish(&fixture.grants[0], QuotaOutcome::Actual(4))
        .await
        .unwrap();
}

#[tokio::test]
async fn postgres_standalone_usage_retries_non_transient_failures() {
    let Some(pool) = pool().await else { return };
    let fixture = TerminalFixture::new(pool.clone()).await;
    let unique = uuid::Uuid::new_v4().simple().to_string();
    // Identifiers contain only fixed text and a locally generated hexadecimal
    // UUID; the trigger argument is the fixture's generated environment id.
    let sequence = format!("chalk_router.test_usage_failures_{unique}");
    let function = format!("chalk_router.test_reject_usage_{unique}");
    let trigger = format!("test_reject_usage_{unique}");
    // Fail the first two inserts with a SQLSTATE outside the transient set: a
    // read-only error after failover can still succeed on a later connection.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE SEQUENCE {sequence}; \
         CREATE FUNCTION {function}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
             IF NEW.environment_id = TG_ARGV[0] THEN \
                 IF nextval('{sequence}') <= 2 THEN \
                     RAISE EXCEPTION 'injected usage failure' USING ERRCODE = '25006'; \
                 END IF; \
             END IF; \
             RETURN NEW; \
         END $$; \
         CREATE TRIGGER {trigger} BEFORE INSERT ON chalk_router.usage_events \
             FOR EACH ROW EXECUTE FUNCTION {function}('{env}')",
        env = fixture.env,
    )))
    .execute(&pool)
    .await
    .unwrap();
    let tracker =
        openai_server::usage_tracker::UsageTracker::new(pool.clone(), fixture.env.clone())
            .await
            .unwrap();
    let result = tracker.record(fixture.record(uuid::Uuid::new_v4())).await;
    let attempts: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT last_value FROM {sequence}"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP TRIGGER {trigger} ON chalk_router.usage_events; \
         DROP FUNCTION {function}(); DROP SEQUENCE {sequence}"
    )))
    .execute(&pool)
    .await
    .unwrap();
    result.expect("non-transient failures are retried within the write budget");
    assert_eq!(attempts, 3, "two rejected inserts, then one durable write");
    let usage: i64 = sqlx::query_scalar(
        "SELECT SUM(total_tokens)::BIGINT FROM chalk_router.usage_events WHERE environment_id=$1",
    )
    .bind(&fixture.env)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(usage, 4);
    // The record carried no permit; release the fixture's outstanding grants.
    for grant in &fixture.grants {
        fixture
            .backend
            .finish(grant, QuotaOutcome::Actual(0))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn postgres_terminal_mixed_memory_and_database_permits_settle() {
    use openai_server::quota::{FixedWindowUsageQuery, QuotaPermit};
    let Some(pool) = pool().await else { return };
    let fixture = TerminalFixture::new(pool.clone()).await;
    let memory: Arc<dyn QuotaBackend> = Arc::new(MemoryQuota::new());
    let action = &fixture.grants[0].actions[0];
    let QuotaDecision::Allowed(Some(memory_grant)) = memory
        .acquire_many(std::slice::from_ref(action), None)
        .await
        .unwrap()
    else {
        panic!("admit memory part")
    };
    let permit = QuotaPermit::merge(
        Some(QuotaPermit::new(memory.clone(), memory_grant)),
        Some(QuotaPermit::new(
            fixture.backend.clone(),
            fixture.grants[1].clone(),
        )),
    );
    let tracker = openai_server::usage_tracker::UsageTracker::new(pool, fixture.env.clone())
        .await
        .unwrap();
    tracker
        .record_with_quota(fixture.record(uuid::Uuid::new_v4()), permit, Some(4))
        .await
        .unwrap();
    let memory_usage = memory
        .fixed_window_usage(&[FixedWindowUsageQuery {
            policy_id: action.policy_id.clone(),
            window_start: action.window_start.unwrap(),
        }])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(memory_usage[&action.policy_id], 4);
    assert_eq!(fixture.balances().await, vec![10, 4]);
    fixture
        .backend
        .finish(&fixture.grants[0], QuotaOutcome::Actual(4))
        .await
        .unwrap();
}

#[tokio::test]
async fn postgres_route_decode_failure_preserves_request_grant() {
    use openai_server::quota::QuotaAdmission;
    let Some(pool) = pool().await else { return };
    let fixture = TerminalFixture::new(pool.clone()).await;
    // Replace only the result encoding, after the real admission has executed.
    // The postgres group serializes tests that temporarily replace this function.
    let migration = include_str!("../../migrations/default/0028_quota_admission.sql");
    let original = migration[migration
        .find("CREATE FUNCTION chalk_router.acquire_request_and_route_quota(")
        .unwrap()..]
        .replacen("CREATE FUNCTION", "CREATE OR REPLACE FUNCTION", 1);
    let broken = original.replace(
        "RETURN QUERY SELECT to_jsonb(v_request), to_jsonb(v_route), NULL::TEXT;",
        "RETURN QUERY SELECT to_jsonb(v_request), '{}'::JSONB, NULL::TEXT;",
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(broken))
        .execute(&pool)
        .await
        .unwrap();
    let mut route = fixture.grants[1].actions.clone();
    route[0].amount = 2000; // Denied: no unobservable route receipt to clean up.
    let result = fixture
        .backend
        .acquire_request_and_route(&fixture.grants[0].actions, &route, None)
        .await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(original))
        .execute(&pool)
        .await
        .unwrap();
    let QuotaAdmission::RequestAllowed {
        request_grant: Some(grant),
        route: Err(_),
    } = result.unwrap()
    else {
        panic!("request grant must survive malformed route decision")
    };
    assert_eq!(fixture.balances().await, vec![20, 10]);
    fixture
        .backend
        .finish(&grant, QuotaOutcome::Actual(0))
        .await
        .unwrap();
    assert_eq!(fixture.balances().await, vec![10, 10]);
}

/// Compare the batched terminal path to the public, ordered per-grant settler.
/// A savepoint makes both operate on exactly the same receipts and states.
#[tokio::test]
async fn postgres_terminal_batch_matches_ordered_settlement() {
    use openai_server::quota::{QuotaAlgorithm, QuotaMeter, QuotaSource};
    let Some(pool) = pool().await else { return };
    for case in [
        "bucket_refund_first",
        "bucket_charge_first",
        "fixed_current",
        "fixed_expired",
        "fixed_newer",
        "null_actual",
        "negative_actual",
        "saturation",
        "duplicate_permit",
        "multi_policy",
    ] {
        let policies = if case == "multi_policy" { 3 } else { 2 };
        let mut fixture = TerminalFixture::with_policies(pool.clone(), policies).await;
        let mut tx = pool.begin().await.unwrap();
        let mut actual = Some(20_i64);
        if case.starts_with("bucket") {
            let action = &mut fixture.grants[0].actions[0];
            action.source = QuotaSource::RateLimit;
            action.algorithm = QuotaAlgorithm::TokenBucket;
            action.burst = Some(100);
            action.limit_value = 1;
            action.amount = 80;
            sqlx::query(
                "UPDATE chalk_router.quota_reservations SET reserved_amount=80 WHERE policy_id=$1",
            )
            .bind(&fixture.policies[0])
            .execute(&mut *tx)
            .await
            .unwrap();
            // Future timestamp suppresses refill until the first settlement.
            sqlx::query("UPDATE chalk_router.quota_states SET balance=95, updated_at=now()+INTERVAL '1 day' WHERE policy_id=$1")
                .bind(&fixture.policies[0]).execute(&mut *tx).await.unwrap();
            let mut overlapping = fixture.grants[0].clone();
            overlapping.permit_id = uuid::Uuid::new_v4();
            overlapping.actions[0].amount = 10;
            sqlx::query("INSERT INTO chalk_router.quota_reservations(policy_id,permit_id,reserved_amount,expires_at) VALUES($1,$2,10,now()+INTERVAL '1 hour')")
                .bind(&fixture.policies[0]).bind(overlapping.permit_id).execute(&mut *tx).await.unwrap();
            fixture.grants.insert(1, overlapping);
            if case == "bucket_charge_first" {
                fixture.grants.swap(0, 1);
            }
        } else if case == "fixed_expired" {
            for grant in &mut fixture.grants {
                grant.actions[0].window_end = Some(Utc::now() - chrono::Duration::seconds(1));
            }
        } else if case == "fixed_newer" {
            sqlx::query("UPDATE chalk_router.quota_states SET window_start=window_start+INTERVAL '1 day', used_amount=7 WHERE policy_id=ANY($1)")
                .bind(&fixture.policies).execute(&mut *tx).await.unwrap();
        } else if case == "null_actual" {
            actual = None;
        } else if case == "negative_actual" {
            actual = Some(-10);
        } else if case == "saturation" {
            sqlx::query("UPDATE chalk_router.quota_states SET used_amount=9223372036854775807 WHERE policy_id=ANY($1)")
                .bind(&fixture.policies).execute(&mut *tx).await.unwrap();
        } else if case == "duplicate_permit" {
            fixture.grants.push(fixture.grants[0].clone());
        }
        // Mix a concurrency release into every case, including unknown actual.
        let last = fixture
            .grants
            .iter_mut()
            .find(|grant| grant.actions[0].policy_id == fixture.policies[1])
            .unwrap();
        let action = &mut last.actions[0];
        action.source = QuotaSource::RateLimit;
        action.algorithm = QuotaAlgorithm::Concurrency;
        action.meter = QuotaMeter::ConcurrentRequests;
        action.amount = 1;
        sqlx::query("DELETE FROM chalk_router.quota_reservations WHERE permit_id=$1")
            .bind(last.permit_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO chalk_router.quota_leases(policy_id,permit_id,expires_at) VALUES($1,$2,now()+INTERVAL '1 hour')")
            .bind(&fixture.policies[1]).bind(last.permit_id).execute(&mut *tx).await.unwrap();
        sqlx::query("UPDATE chalk_router.quota_states SET active_count=1 WHERE policy_id=$1")
            .bind(&fixture.policies[1])
            .execute(&mut *tx)
            .await
            .unwrap();
        if case == "multi_policy" {
            // Production grants carry several policies: fold the lease grant and
            // a second reservation into the first grant, so one batched update
            // joins two reservations and a lease.
            let first = fixture.grants[0].permit_id;
            for grant in fixture.grants.split_off(1) {
                for statement in [
                    "UPDATE chalk_router.quota_reservations SET permit_id=$1 WHERE permit_id=$2",
                    "UPDATE chalk_router.quota_leases SET permit_id=$1 WHERE permit_id=$2",
                ] {
                    sqlx::query(statement)
                        .bind(first)
                        .bind(grant.permit_id)
                        .execute(&mut *tx)
                        .await
                        .unwrap();
                }
                fixture.grants[0].actions.extend(grant.actions);
            }
            let held: (i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM chalk_router.quota_reservations WHERE permit_id=$1), (SELECT count(*) FROM chalk_router.quota_leases WHERE permit_id=$1)")
                .bind(first).fetch_one(&mut *tx).await.unwrap();
            assert_eq!(
                held,
                (2, 1),
                "one grant must hold two reservations and a lease"
            );
        }
        sqlx::query("SAVEPOINT before_settlement")
            .execute(&mut *tx)
            .await
            .unwrap();
        for grant in &fixture.grants {
            sqlx::query("SELECT chalk_router.finish_quota($1,$2,$3,FALSE)")
                .bind(sqlx::types::Json(&grant.actions))
                .bind(grant.permit_id)
                .bind(actual)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        type State = (String, Option<f64>, Option<chrono::DateTime<Utc>>, i64, i64);
        let expected: Vec<State> = sqlx::query_as("SELECT policy_id,balance::DOUBLE PRECISION,window_start,used_amount,active_count FROM chalk_router.quota_states WHERE policy_id=ANY($1) ORDER BY policy_id")
            .bind(&fixture.policies).fetch_all(&mut *tx).await.unwrap();
        sqlx::query("ROLLBACK TO SAVEPOINT before_settlement")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("SELECT chalk_router.finish_quota_usage($1,$2,$3,now(),$4,'terminal-key',NULL,'terminal-connection','terminal-model',2,2,0,0,0,4,1::SMALLINT,NULL)")
            .bind(sqlx::types::Json(&fixture.grants)).bind(actual).bind(uuid::Uuid::new_v4()).bind(&fixture.env)
            .execute(&mut *tx).await.unwrap();
        let got: Vec<State> = sqlx::query_as("SELECT policy_id,balance::DOUBLE PRECISION,window_start,used_amount,active_count FROM chalk_router.quota_states WHERE policy_id=ANY($1) ORDER BY policy_id")
            .bind(&fixture.policies).fetch_all(&mut *tx).await.unwrap();
        for (expected, got) in expected.iter().zip(&got) {
            assert_eq!(
                (&expected.0, &expected.2, expected.3, expected.4),
                (&got.0, &got.2, got.3, got.4),
                "{case}"
            );
            match (expected.1, got.1) {
                (Some(a), Some(b)) => assert!((a - b).abs() < 0.02, "{case}: balance {a} != {b}"),
                (a, b) => assert_eq!(a, b, "{case}"),
            }
        }
        if case.starts_with("bucket") {
            let balance = got[0].1.unwrap();
            let expected = if case == "bucket_refund_first" {
                90.0
            } else {
                100.0
            };
            assert!(
                (balance - expected).abs() < 0.02,
                "order must preserve burst caps: {case}"
            );
        }
        let remaining: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM chalk_router.quota_reservations WHERE policy_id=ANY($1)) + (SELECT count(*) FROM chalk_router.quota_leases WHERE policy_id=ANY($1))")
            .bind(&fixture.policies).fetch_one(&mut *tx).await.unwrap();
        assert_eq!(remaining, 0, "{case}");
        tx.rollback().await.unwrap();
    }
}

/// A lock wait past the admission `lock_timeout` is reported as contention,
/// not as an unavailable backend that would fail open, and leaves no receipt.
#[tokio::test]
async fn postgres_admission_lock_timeout_reports_contention() {
    let Some(pool) = pool().await else { return };
    let fixture = TerminalFixture::new(pool.clone()).await;
    let action = fixture.grants[0].actions[0].clone();
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM chalk_router.quota_states WHERE policy_id=$1 FOR UPDATE")
        .bind(&fixture.policies[0])
        .execute(&mut *holder)
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let result = fixture.backend.acquire_many(&[action], None).await;
    let waited = started.elapsed();
    holder.rollback().await.unwrap();
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("admission must not succeed while the state row is held"),
    };
    assert!(error.is_contended(), "{error}");
    assert!(!error.is_pool_exhausted());
    assert!(
        waited < std::time::Duration::from_secs(4),
        "lock wait must be bounded: {waited:?}"
    );
    let receipts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM chalk_router.quota_reservations WHERE policy_id=$1",
    )
    .bind(&fixture.policies[0])
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(receipts, 1, "only the fixture's own receipt remains");
    // The function-scoped setting must not leak into the pooled session.
    let timeout: String = sqlx::query_scalar("SHOW lock_timeout")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(timeout, "0");
}
