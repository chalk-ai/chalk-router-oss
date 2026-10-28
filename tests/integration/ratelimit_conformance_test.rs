//! Shared conformance tests for backend-neutral quota state.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use openai_server::quota::{
    BackgroundOwner, FixedWindowUsageQuery, MemoryQuota, PostgresQuota, QuotaAction,
    QuotaAlgorithm, QuotaBackend, QuotaDecision, QuotaMeter, QuotaOutcome, QuotaSource,
};
use openai_server::ratelimit::{
    MemoryRateLimitPolicyStore, PostgresRateLimitPolicyStore, RateLimitMatch, RateLimitModel,
    RateLimitPolicy, RateLimitPolicyStore,
};
use openai_server::{
    backend::RouteResolution, budget::MemoryBudgetPolicyStore, quota::TokenReservation,
    quota_policy::QuotaPolicyResolver,
};

fn action(
    id: &str,
    source: QuotaSource,
    algorithm: QuotaAlgorithm,
    meter: QuotaMeter,
    limit: u64,
    burst: Option<u64>,
    amount: u64,
) -> QuotaAction {
    QuotaAction {
        policy_id: id.to_string(),
        environment_id: "quota-conformance".to_string(),
        source,
        algorithm,
        meter,
        limit_value: limit,
        burst,
        amount,
        window_start: None,
        window_end: None,
    }
}

fn at(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds, 0).expect("valid timestamp")
}

fn rate_policy(id: &str, created_at: DateTime<Utc>) -> RateLimitPolicy {
    RateLimitPolicy {
        quota_policy_id: format!("quota-{id}"),
        id: id.to_string(),
        limit_type: "requests_per_minute".to_string(),
        limit_value: 10,
        burst_value: Some(20),
        connection_id: Some("conn-1".to_string()),
        models: if id.ends_with("p1") {
            vec!["gpt-4o".to_string()]
        } else {
            vec!["gpt-4o".to_string(), id.to_string()]
        },
        scope: "usage_pool".to_string(),
        scope_id: "pool-1".to_string(),
        enabled: true,
        created_at,
    }
}

async fn run_policy_store_suite(store: Arc<dyn RateLimitPolicyStore>, label: &str) {
    let store: Arc<dyn RateLimitPolicyStore> =
        Arc::new(openai_server::quota_definition_cache::CachedRateLimitPolicyStore::new(store));
    store
        .create(rate_policy(&format!("{label}-p2"), at(2_000)))
        .await
        .unwrap();
    store
        .create(rate_policy(&format!("{label}-p1"), at(1_000)))
        .await
        .unwrap();
    store
        .create(rate_policy(&format!("{label}-p3"), at(3_000)))
        .await
        .unwrap();

    let listed = store.list().await;
    assert_eq!(listed.len(), 3, "{label}: list includes every source row");
    assert_eq!(listed[0].id, format!("{label}-p1"));
    assert_eq!(listed[0].burst_value, Some(20));
    assert_eq!(listed[0].connection_id.as_deref(), Some("conn-1"));
    assert_eq!(listed[0].models, ["gpt-4o"]);
    assert!(!listed[0].quota_policy_id.is_empty());

    let page = store.list_page(at(3_000), 10).await.unwrap();
    assert_eq!(page.len(), 2, "{label}: page boundary is strict");
    assert_eq!(page[0].id, format!("{label}-p2"));

    let matches = store
        .matching(&RateLimitMatch {
            key_hash: "key-1",
            usage_pool_id: Some("pool-1"),
            connection_id: Some("conn-1"),
            model: RateLimitModel::Exact("gpt-4o"),
        })
        .await
        .unwrap();
    assert_eq!(matches.len(), 3, "{label}: selector match");
    let unspecified_model = store
        .matching(&RateLimitMatch {
            key_hash: "key-1",
            usage_pool_id: Some("pool-1"),
            connection_id: Some("conn-1"),
            model: RateLimitModel::ProviderSelected,
        })
        .await
        .unwrap();
    assert_eq!(
        unspecified_model.len(),
        3,
        "{label}: provider-selected models expose model-scoped policies to admission"
    );
    let model_not_applicable = store
        .matching(&RateLimitMatch {
            key_hash: "key-1",
            usage_pool_id: Some("pool-1"),
            connection_id: Some("conn-1"),
            model: RateLimitModel::NotApplicable,
        })
        .await
        .unwrap();
    assert!(
        model_not_applicable.is_empty(),
        "{label}: model-less operations exclude model-scoped policies"
    );
    let misses = store
        .matching(&RateLimitMatch {
            key_hash: "key-1",
            usage_pool_id: Some("pool-1"),
            connection_id: Some("conn-2"),
            model: RateLimitModel::Exact("gpt-4o"),
        })
        .await
        .unwrap();
    assert!(misses.is_empty(), "{label}: connection selector mismatch");

    let mut global = rate_policy(&format!("{label}-global"), at(4_000));
    global.connection_id = None;
    global.models.clear();
    global.scope = "api_key".to_string();
    global.scope_id = "key-1".to_string();
    store.create(global).await.unwrap();
    let mut pool_global = rate_policy(&format!("{label}-pool-global"), at(4_001));
    pool_global.connection_id = None;
    pool_global.models.clear();
    store.create(pool_global).await.unwrap();
    let global_matches = store
        .matching(&RateLimitMatch {
            key_hash: "key-1",
            usage_pool_id: Some("pool-1"),
            connection_id: None,
            model: RateLimitModel::NotApplicable,
        })
        .await
        .unwrap();
    assert_eq!(
        global_matches.len(),
        2,
        "{label}: direct key and inherited pool globals both match"
    );
    assert!(
        matches.iter().all(|policy| policy.connection_id.is_some()),
        "{label}: dispatch matching excludes global policies"
    );
    store.delete(&format!("{label}-global")).await.unwrap();
    store.delete(&format!("{label}-pool-global")).await.unwrap();

    let mut policy = store.get(&format!("{label}-p1")).await.unwrap();
    policy.enabled = false;
    let updated = store.update(policy).await.unwrap();
    assert!(!updated.enabled, "{label}: disable persists");
    let request = RateLimitMatch {
        key_hash: "key-1",
        usage_pool_id: Some("pool-1"),
        connection_id: Some("conn-1"),
        model: RateLimitModel::Exact("gpt-4o"),
    };
    assert_eq!(
        store.matching_shared(&request).await.unwrap().len(),
        2,
        "{label}: disabled policy invalidates cached match"
    );
    let mut enabled = updated;
    enabled.enabled = true;
    enabled.limit_value = 99;
    store.update(enabled).await.unwrap();
    let matches = store.matching_shared(&request).await.unwrap();
    assert_eq!(matches.len(), 3, "{label}: enable invalidates cached match");
    assert_eq!(
        matches
            .iter()
            .find(|p| p.id == format!("{label}-p1"))
            .unwrap()
            .limit_value,
        99
    );
    store.delete(&format!("{label}-p1")).await.unwrap();
    store.delete(&format!("{label}-p1")).await.unwrap();
    assert_eq!(store.list().await.len(), 2, "{label}: delete is idempotent");
    assert_eq!(
        store.matching_shared(&request).await.unwrap().len(),
        2,
        "{label}: deletion invalidates cached match"
    );
}

async fn run_policy_identity_suite(store: Arc<dyn RateLimitPolicyStore>, label: &str) {
    let mut first = rate_policy(&format!("{label}-identity-first"), at(5_000));
    first.scope_id = format!("{label}-identity-target");
    first.models = vec![
        "model-b".to_string(),
        "model-a".to_string(),
        "model-a".to_string(),
    ];
    first.enabled = false;
    let first = store.create(first).await.expect("create disabled identity");
    assert_eq!(first.models, ["model-a", "model-b"]);

    let mut duplicate = rate_policy(&format!("{label}-identity-duplicate"), at(5_001));
    duplicate.scope_id = first.scope_id.clone();
    duplicate.models = vec!["model-b".to_string(), "model-a".to_string()];
    let error = store
        .create(duplicate)
        .await
        .expect_err("model order and enabled state do not change identity");
    assert!(error
        .to_string()
        .contains("Edit the existing rate limit instead"));

    let mut different_models = rate_policy(&format!("{label}-identity-other"), at(5_002));
    different_models.scope_id = first.scope_id.clone();
    different_models.models = vec!["model-c".to_string()];
    let different_models = store
        .create(different_models)
        .await
        .expect("different model selector remains valid");

    let mut self_update = first.clone();
    self_update.limit_value = 99;
    assert_eq!(
        store
            .update(self_update)
            .await
            .expect("self update")
            .limit_value,
        99
    );

    let mut ownership_update = store.get(&first.id).await.expect("read policy");
    ownership_update.scope = "api_key".to_string();
    ownership_update.scope_id = format!("{label}-different-target");
    let ownership_update = store
        .update(ownership_update)
        .await
        .expect("configuration update");
    assert_eq!(ownership_update.scope, "usage_pool");
    assert_eq!(ownership_update.scope_id, first.scope_id);

    let mut conflicting_update = different_models.clone();
    conflicting_update.models = vec!["model-b".to_string(), "model-a".to_string()];
    store
        .update(conflicting_update)
        .await
        .expect_err("update into another identity must fail");
    assert_eq!(
        store
            .get(&different_models.id)
            .await
            .expect("failed update leaves row intact")
            .models,
        ["model-c"]
    );

    let mut race_a = rate_policy(&format!("{label}-race-a"), at(5_003));
    race_a.scope_id = format!("{label}-race-target");
    race_a.models = vec!["model-race".to_string()];
    let mut race_b = race_a.clone();
    race_b.id = format!("{label}-race-b");
    race_b.quota_policy_id = format!("quota-{label}-race-b");
    let (left, right) = tokio::join!(store.create(race_a), store.create(race_b));
    assert_ne!(
        left.is_ok(),
        right.is_ok(),
        "{label}: exactly one racing create wins"
    );

    for id in [
        first.id,
        different_models.id,
        format!("{label}-race-a"),
        format!("{label}-race-b"),
    ] {
        store.delete(&id).await.unwrap();
    }
}

#[tokio::test]
async fn memory_policy_store_satisfies_conformance() {
    let store: Arc<dyn RateLimitPolicyStore> =
        Arc::new(MemoryRateLimitPolicyStore::new("store-memory".to_string()));
    run_policy_store_suite(store.clone(), "memory-store").await;
    run_policy_identity_suite(store, "memory-store").await;
}

fn fixed_action(id: &str, amount: u64) -> QuotaAction {
    let now = Utc::now();
    fixed_action_for_window(
        id,
        amount,
        now - chrono::Duration::minutes(1),
        now + chrono::Duration::days(1),
    )
}

fn fixed_action_for_window(
    id: &str,
    amount: u64,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> QuotaAction {
    QuotaAction {
        window_start: Some(window_start),
        window_end: Some(window_end),
        ..action(
            id,
            QuotaSource::BudgetLimit,
            QuotaAlgorithm::FixedWindow,
            QuotaMeter::Tokens,
            100,
            None,
            amount,
        )
    }
}

async fn fixed_window_late_settlement_cannot_mutate_new_window(
    backend: Arc<dyn QuotaBackend>,
    policy_id: &str,
) {
    let now = Utc::now();
    let previous_window = fixed_action_for_window(
        policy_id,
        80,
        now - chrono::Duration::days(2),
        now - chrono::Duration::days(1),
    );
    let current_window = fixed_action_for_window(
        policy_id,
        20,
        now - chrono::Duration::days(1),
        now + chrono::Duration::days(1),
    );

    let previous_grant = match backend
        .acquire_many(std::slice::from_ref(&previous_window), None)
        .await
        .unwrap()
    {
        QuotaDecision::Allowed(Some(grant)) => grant,
        other => panic!("expected previous-window grant, got {other:?}"),
    };
    let current_grant = match backend
        .acquire_many(std::slice::from_ref(&current_window), None)
        .await
        .unwrap()
    {
        QuotaDecision::Allowed(Some(grant)) => grant,
        other => panic!("expected current-window grant, got {other:?}"),
    };

    // The first permit finishes after acquisition has advanced the state to a
    // new calendar window. Its settlement must not refund the new window.
    backend
        .finish(&previous_grant, QuotaOutcome::Actual(10))
        .await
        .unwrap();

    let probe = QuotaAction {
        amount: 81,
        ..current_window.clone()
    };
    assert!(matches!(
        backend.acquire_many(&[probe], None).await.unwrap(),
        QuotaDecision::Limited(_)
    ));

    backend
        .finish(&current_grant, QuotaOutcome::Unknown)
        .await
        .unwrap();
}

/// A soft-cap request can finish after its window closes. Its excess over the
/// reservation must land in the next window, whether or not an admission has
/// opened that window yet.
async fn fixed_window_late_excess_carries_into_the_next_window(
    backend: Arc<dyn QuotaBackend>,
    rolled_policy: &str,
    unrolled_policy: &str,
) {
    let now = Utc::now();
    let closed = |policy: &str, amount| {
        fixed_action_for_window(
            policy,
            amount,
            now - chrono::Duration::days(2),
            now - chrono::Duration::days(1),
        )
    };
    let open = |policy: &str, amount| {
        fixed_action_for_window(
            policy,
            amount,
            now - chrono::Duration::days(1),
            now + chrono::Duration::days(1),
        )
    };
    let acquire = |action: QuotaAction| {
        let backend = backend.clone();
        async move { backend.acquire_many(&[action], None).await.unwrap() }
    };
    let grant = |action: QuotaAction| {
        let acquire = acquire.clone();
        async move {
            match acquire(action).await {
                QuotaDecision::Allowed(Some(grant)) => grant,
                other => panic!("expected a grant, got {other:?}"),
            }
        }
    };

    // A later admission already opened the next window: 20 + (70 - 30).
    let late = grant(closed(rolled_policy, 30)).await;
    let current = grant(open(rolled_policy, 20)).await;
    backend
        .finish(&late, QuotaOutcome::Actual(70))
        .await
        .unwrap();
    assert!(matches!(
        acquire(open(rolled_policy, 41)).await,
        QuotaDecision::Limited(_)
    ));
    assert!(matches!(
        acquire(open(rolled_policy, 40)).await,
        QuotaDecision::Allowed(Some(_))
    ));
    backend
        .finish(&current, QuotaOutcome::Unknown)
        .await
        .unwrap();

    // No admission has opened the next window: settlement opens it with 40.
    let late = grant(closed(unrolled_policy, 30)).await;
    backend
        .finish(&late, QuotaOutcome::Actual(70))
        .await
        .unwrap();
    assert!(matches!(
        acquire(open(unrolled_policy, 61)).await,
        QuotaDecision::Limited(_)
    ));
    assert!(matches!(
        acquire(open(unrolled_policy, 60)).await,
        QuotaDecision::Allowed(Some(_))
    ));
}

async fn run_suite(backend: Arc<dyn QuotaBackend>, prefix: &str) {
    let rpm = action(
        &format!("{prefix}-rpm"),
        QuotaSource::RateLimit,
        QuotaAlgorithm::TokenBucket,
        QuotaMeter::Requests,
        60,
        Some(2),
        1,
    );
    for _ in 0..2 {
        assert!(matches!(
            backend
                .acquire_many(std::slice::from_ref(&rpm), None)
                .await
                .unwrap(),
            QuotaDecision::Allowed(None)
        ));
    }
    assert!(matches!(
        backend
            .acquire_many(std::slice::from_ref(&rpm), None)
            .await
            .unwrap(),
        QuotaDecision::Limited(_)
    ));

    let budget = fixed_action(&format!("{prefix}-budget"), 80);
    let grant = match backend
        .acquire_many(std::slice::from_ref(&budget), None)
        .await
        .unwrap()
    {
        QuotaDecision::Allowed(Some(grant)) => grant,
        other => panic!("expected budget grant, got {other:?}"),
    };
    backend
        .finish(&grant, QuotaOutcome::Actual(20))
        .await
        .unwrap();
    // Repeating finish must not apply the refund twice.
    backend
        .finish(&grant, QuotaOutcome::Actual(20))
        .await
        .unwrap();
    assert!(matches!(
        backend
            .acquire_many(std::slice::from_ref(&budget), None)
            .await
            .unwrap(),
        QuotaDecision::Allowed(Some(_))
    ));

    let concurrency = action(
        &format!("{prefix}-concurrency"),
        QuotaSource::RateLimit,
        QuotaAlgorithm::Concurrency,
        QuotaMeter::ConcurrentRequests,
        1,
        None,
        1,
    );
    let grant = match backend
        .acquire_many(std::slice::from_ref(&concurrency), None)
        .await
        .unwrap()
    {
        QuotaDecision::Allowed(Some(grant)) => grant,
        other => panic!("expected concurrency grant, got {other:?}"),
    };
    assert!(matches!(
        backend
            .acquire_many(std::slice::from_ref(&concurrency), None)
            .await
            .unwrap(),
        QuotaDecision::Limited(_)
    ));
    backend.finish(&grant, QuotaOutcome::Unknown).await.unwrap();
    assert!(matches!(
        backend
            .acquire_many(std::slice::from_ref(&concurrency), None)
            .await
            .unwrap(),
        QuotaDecision::Allowed(Some(_))
    ));

    // All-or-nothing: the fresh bucket must not be charged when the exhausted
    // bucket rejects the same acquisition.
    let fresh = action(
        &format!("{prefix}-fresh"),
        QuotaSource::RateLimit,
        QuotaAlgorithm::TokenBucket,
        QuotaMeter::Requests,
        1,
        Some(1),
        1,
    );
    let blocked = action(
        &format!("{prefix}-blocked"),
        QuotaSource::RateLimit,
        QuotaAlgorithm::TokenBucket,
        QuotaMeter::Requests,
        1,
        Some(1),
        2,
    );
    assert!(matches!(
        backend
            .acquire_many(&[blocked, fresh.clone()], None)
            .await
            .unwrap(),
        QuotaDecision::Limited(_)
    ));
    assert!(matches!(
        backend.acquire_many(&[fresh], None).await.unwrap(),
        QuotaDecision::Allowed(None)
    ));
}

#[tokio::test]
async fn memory_quota_satisfies_conformance() {
    run_suite(Arc::new(MemoryQuota::new()), "memory").await;
}

#[tokio::test]
async fn memory_fixed_window_late_settlement_does_not_mutate_new_window() {
    fixed_window_late_settlement_cannot_mutate_new_window(
        Arc::new(MemoryQuota::new()),
        "memory-fixed-window-rollover",
    )
    .await;
}

#[tokio::test]
async fn memory_fixed_window_late_excess_carries_into_the_next_window() {
    fixed_window_late_excess_carries_into_the_next_window(
        Arc::new(MemoryQuota::new()),
        "memory-late-excess-rolled",
        "memory-late-excess-unrolled",
    )
    .await;
}

async fn postgres_pool() -> Option<sqlx::PgPool> {
    let database_url = std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|value| !value.is_empty())?;
    openai_server::migrate::run(&database_url)
        .await
        .expect("apply router migrations");
    Some(
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_with(
                openai_server::database::connect_options(&database_url)
                    .expect("parse disposable PostgreSQL test URL"),
            )
            .await
            .expect("connect to PostgreSQL"),
    )
}

#[tokio::test]
async fn postgres_policy_store_satisfies_conformance() {
    let Some(pool) = postgres_pool().await else {
        eprintln!("skipping PostgreSQL policy-store conformance; set TEST_DATABASE_URL");
        return;
    };
    let environment_id = format!("rate-store-{}", uuid::Uuid::new_v4());
    sqlx::query(
        "INSERT INTO chalk_router.provider_connections \
         (environment_id, id, name, provider_kind, prefix, api_key_envelope) \
         VALUES ($1, 'conn-1', 'test', 'openai', 'test/', $2)",
    )
    .bind(&environment_id)
    .bind(vec![0_u8])
    .execute(&pool)
    .await
    .expect("seed rate-limit target connection");
    let store: Arc<dyn RateLimitPolicyStore> = Arc::new(PostgresRateLimitPolicyStore::new(
        pool.clone(),
        environment_id.clone(),
    ));
    run_policy_store_suite(store.clone(), &environment_id).await;
    run_policy_identity_suite(store, &environment_id).await;

    let (sources, bridges): (i64, i64) = sqlx::query_as(
        "SELECT \
            (SELECT count(*) FROM chalk_router.rate_limits WHERE environment_id = $1), \
            (SELECT count(*) FROM chalk_router.quota_policies WHERE environment_id = $1)",
    )
    .bind(&environment_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((sources, bridges), (2, 2));

    let error = sqlx::query(
        "DELETE FROM chalk_router.provider_connections \
         WHERE environment_id = $1 AND id = 'conn-1'",
    )
    .bind(&environment_id)
    .execute(&pool)
    .await
    .expect_err("a referenced rate-limit connection must be restricted");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );
}

fn parity_policy(
    environment_id: &str,
    id: &str,
    connection_id: Option<&str>,
    models: &[&str],
    scope: &str,
    scope_id: &str,
) -> RateLimitPolicy {
    RateLimitPolicy {
        // `quota_policies.id` is a global (not environment-scoped) primary
        // key, unlike `rate_limits.id`, so this must stay unique across runs
        // sharing one database, not just within this test's environment.
        quota_policy_id: format!("quota-{environment_id}-{id}"),
        id: id.to_string(),
        limit_type: "requests_per_minute".to_string(),
        limit_value: 10,
        burst_value: None,
        connection_id: connection_id.map(str::to_string),
        models: models.iter().map(|model| model.to_string()).collect(),
        scope: scope.to_string(),
        scope_id: scope_id.to_string(),
        enabled: true,
        created_at: at(9_000),
    }
}

async fn canonical_alias_quota_suite(
    store: Arc<dyn RateLimitPolicyStore>,
    quota: Arc<dyn QuotaBackend>,
    label: &str,
) {
    let resolver = QuotaPolicyResolver::new(
        label.to_string(),
        Arc::new(MemoryBudgetPolicyStore::new(label.to_string())),
        store.clone(),
    );
    let resolution = RouteResolution {
        provider_kind: "openai".into(),
        connection_id: Some("conn-1".into()),
        connection_name: "openai".into(),
        upstream_model_id: "gpt-4o".into(),
        connection_prefix: Some("openai/".into()),
        model_route_id: None,
    };
    for scope in ["api_key", "usage_pool"] {
        for limit_type in [
            "requests_per_minute",
            "tokens_per_minute",
            "concurrent_requests",
        ] {
            for models in [
                &["gpt-4o"][..],
                &["openai/gpt-4o"][..],
                &["gpt-4o", "openai/gpt-4o"][..],
            ] {
                let id = format!("{label}-{scope}-{limit_type}-{}", models.join("+"));
                let mut policy = parity_policy(label, &id, Some("conn-1"), models, scope, &id);
                policy.limit_type = limit_type.into();
                policy.limit_value = 1;
                policy.burst_value = (limit_type != "concurrent_requests").then_some(1);
                let policy = store.create(policy).await.unwrap();
                let request = |public_model_id| RateLimitMatch {
                    key_hash: &id,
                    usage_pool_id: Some(&id),
                    connection_id: Some("conn-1"),
                    model: RateLimitModel::Resolved {
                        public_model_id,
                        resolution: &resolution,
                    },
                };
                let bare = resolver
                    .rate_actions(&request("gpt-4o"), TokenReservation::Known(1))
                    .await
                    .unwrap();
                let prefixed = resolver
                    .rate_actions(&request("openai/gpt-4o"), TokenReservation::Known(1))
                    .await
                    .unwrap();
                for plan in [&bare, &prefixed] {
                    assert_eq!(plan.actions.len(), 1, "{id}: match each policy once");
                    assert_eq!(plan.actions[0].policy_id, policy.quota_policy_id);
                }
                let first = quota.acquire_many(&bare.actions, None).await.unwrap();
                let QuotaDecision::Allowed(grant) = first else {
                    panic!("{id}: first admission denied")
                };
                assert!(
                    matches!(
                        quota.acquire_many(&prefixed.actions, None).await.unwrap(),
                        QuotaDecision::Limited(_)
                    ),
                    "{id}: aliases must share the same counter"
                );
                if let Some(grant) = grant {
                    quota.finish(&grant, QuotaOutcome::Actual(1)).await.unwrap();
                }
                if limit_type == "concurrent_requests" {
                    let QuotaDecision::Allowed(Some(grant)) =
                        quota.acquire_many(&prefixed.actions, None).await.unwrap()
                    else {
                        panic!("{id}: concurrency permit was not released")
                    };
                    quota.finish(&grant, QuotaOutcome::Actual(1)).await.unwrap();
                }
                // Equivalent strings must never transfer a policy to a different
                // connection, key/pool, custom prefix, or chalk route alias.
                for (public_model_id, route) in [
                    (
                        "gpt-4o",
                        RouteResolution {
                            connection_prefix: Some("custom/".into()),
                            model_route_id: None,
                            ..resolution.clone()
                        },
                    ),
                    (
                        "gpt-4o",
                        RouteResolution {
                            connection_prefix: None,
                            model_route_id: None,
                            ..resolution.clone()
                        },
                    ),
                    ("chalk/assistant", resolution.clone()),
                ] {
                    let matched = store
                        .matching(&RateLimitMatch {
                            key_hash: &id,
                            usage_pool_id: Some(&id),
                            connection_id: Some("conn-1"),
                            model: RateLimitModel::Resolved {
                                public_model_id,
                                resolution: &route,
                            },
                        })
                        .await
                        .unwrap();
                    let exact_expected = models.contains(&public_model_id);
                    assert_eq!(
                        matched.len(),
                        usize::from(exact_expected),
                        "{id}: aliases remain exact outside canonical shorthand"
                    );
                }
                assert!(store
                    .matching(&RateLimitMatch {
                        connection_id: Some("conn-2"),
                        ..request("gpt-4o")
                    })
                    .await
                    .unwrap()
                    .is_empty());
                assert!(store
                    .matching(&RateLimitMatch {
                        key_hash: "unrelated",
                        usage_pool_id: None,
                        ..request("gpt-4o")
                    })
                    .await
                    .unwrap()
                    .is_empty());
            }
        }
    }
    let id = format!("{label}-nested-id");
    store
        .create(parity_policy(
            label,
            &id,
            Some("conn-1"),
            &["org/model"],
            "api_key",
            &id,
        ))
        .await
        .unwrap();
    let nested_route = RouteResolution {
        upstream_model_id: "org/model".into(),
        ..resolution
    };
    for (public_model_id, expected) in [("org/model", 1), ("openai/org/model", 0)] {
        let matched = store
            .matching(&RateLimitMatch {
                key_hash: &id,
                usage_pool_id: None,
                connection_id: Some("conn-1"),
                model: RateLimitModel::Resolved {
                    public_model_id,
                    resolution: &nested_route,
                },
            })
            .await
            .unwrap();
        assert_eq!(
            matched.len(),
            expected,
            "{label}: slash-containing bare IDs are exact selectors"
        );
    }
}

#[tokio::test]
async fn memory_canonical_model_aliases_share_rate_limit_counters() {
    canonical_alias_quota_suite(
        Arc::new(MemoryRateLimitPolicyStore::new("alias-memory".into())),
        Arc::new(MemoryQuota::new()),
        "alias-memory",
    )
    .await;
}

#[tokio::test]
async fn postgres_canonical_model_aliases_share_rate_limit_counters() {
    let Some(pool) = postgres_pool().await else {
        eprintln!("skipping PostgreSQL canonical-alias conformance; set TEST_DATABASE_URL");
        return;
    };
    let environment_id = format!("alias-pg-{}", uuid::Uuid::new_v4());
    sqlx::query(
        "INSERT INTO chalk_router.provider_connections \
         (environment_id, id, name, provider_kind, prefix, api_key_envelope) \
         VALUES ($1, 'conn-1', 'openai', 'openai', 'openai/', $2)",
    )
    .bind(&environment_id)
    .bind(vec![0_u8])
    .execute(&pool)
    .await
    .unwrap();
    canonical_alias_quota_suite(
        Arc::new(PostgresRateLimitPolicyStore::new(
            pool.clone(),
            environment_id.clone(),
        )),
        Arc::new(PostgresQuota::new(pool)),
        &environment_id,
    )
    .await;
}

/// Table-driven parity check between the two `RateLimitPolicyStore::matching`
/// implementations. `run_policy_store_suite` above exercises each backend's
/// business behavior in isolation; this test seeds the *same* fixture
/// policies into a `MemoryRateLimitPolicyStore` and a
/// `PostgresRateLimitPolicyStore`, runs identical `RateLimitMatch` selectors
/// against both, and asserts they return the same sorted policy-id set. It
/// exists because the SQL `WHERE` clause in `PostgresRateLimitPolicyStore::matching`
/// and the `Iterator::filter` in `MemoryRateLimitPolicyStore::matching` encode
/// the same rules (empty `models`/absent `connection_id` bypass the model
/// check, `ProviderSelected` always matches, `NotApplicable` never matches a
/// model-scoped policy) as two independent expressions with no shared code —
/// a future edit to one without the other would otherwise only be caught by
/// manual review.
#[tokio::test]
async fn postgres_policy_matching_agrees_with_memory_store() {
    let Some(pool) = postgres_pool().await else {
        eprintln!("skipping PostgreSQL/memory rate-limit matching parity; set TEST_DATABASE_URL");
        return;
    };
    let environment_id = format!("rate-parity-{}", uuid::Uuid::new_v4());
    for connection in ["conn-1", "conn-2"] {
        sqlx::query(
            "INSERT INTO chalk_router.provider_connections \
             (environment_id, id, name, provider_kind, prefix, api_key_envelope) \
             VALUES ($1, $2, $2, 'openai', $3, $4)",
        )
        .bind(&environment_id)
        .bind(connection)
        .bind(format!("{connection}/"))
        .bind(vec![0_u8])
        .execute(&pool)
        .await
        .expect("seed rate-limit target connection");
    }

    let fixtures: [(&str, Option<&str>, &[&str], &str, &str); 6] = [
        (
            "parity-conn1-exact",
            Some("conn-1"),
            &["gpt-4o"],
            "api_key",
            "key-1",
        ),
        (
            "parity-conn1-other-model",
            Some("conn-1"),
            &["claude-3"],
            "api_key",
            "key-1",
        ),
        (
            "parity-conn1-empty-models",
            Some("conn-1"),
            &[],
            "usage_pool",
            "pool-1",
        ),
        (
            "parity-conn2-exact",
            Some("conn-2"),
            &["gpt-4o"],
            "api_key",
            "key-1",
        ),
        ("parity-global-key", None, &[], "api_key", "key-1"),
        ("parity-global-pool", None, &[], "usage_pool", "pool-1"),
    ];

    let memory: Arc<dyn RateLimitPolicyStore> =
        Arc::new(MemoryRateLimitPolicyStore::new(environment_id.clone()));
    let postgres: Arc<dyn RateLimitPolicyStore> = Arc::new(PostgresRateLimitPolicyStore::new(
        pool.clone(),
        environment_id.clone(),
    ));
    for (id, connection_id, models, scope, scope_id) in fixtures {
        let policy = parity_policy(&environment_id, id, connection_id, models, scope, scope_id);
        memory
            .create(policy.clone())
            .await
            .expect("seed memory fixture");
        postgres
            .create(policy)
            .await
            .expect("seed postgres fixture");
    }

    let scenarios: [(&str, RateLimitMatch<'_>, &[&str]); 9] = [
        (
            "conn-1 exact match hits the exact-model and open policies",
            RateLimitMatch {
                key_hash: "key-1",
                usage_pool_id: Some("pool-1"),
                connection_id: Some("conn-1"),
                model: RateLimitModel::Exact("gpt-4o"),
            },
            &["parity-conn1-empty-models", "parity-conn1-exact"],
        ),
        (
            "conn-1 exact match on the other policy's model",
            RateLimitMatch {
                key_hash: "key-1",
                usage_pool_id: Some("pool-1"),
                connection_id: Some("conn-1"),
                model: RateLimitModel::Exact("claude-3"),
            },
            &["parity-conn1-empty-models", "parity-conn1-other-model"],
        ),
        (
            "conn-1 exact match on an unlisted model only hits the open policy",
            RateLimitMatch {
                key_hash: "key-1",
                usage_pool_id: Some("pool-1"),
                connection_id: Some("conn-1"),
                model: RateLimitModel::Exact("mistral"),
            },
            &["parity-conn1-empty-models"],
        ),
        (
            "conn-1 provider-selected exposes every model-scoped policy",
            RateLimitMatch {
                key_hash: "key-1",
                usage_pool_id: Some("pool-1"),
                connection_id: Some("conn-1"),
                model: RateLimitModel::ProviderSelected,
            },
            &[
                "parity-conn1-empty-models",
                "parity-conn1-exact",
                "parity-conn1-other-model",
            ],
        ),
        (
            "conn-1 model-not-applicable excludes model-scoped policies",
            RateLimitMatch {
                key_hash: "key-1",
                usage_pool_id: Some("pool-1"),
                connection_id: Some("conn-1"),
                model: RateLimitModel::NotApplicable,
            },
            &["parity-conn1-empty-models"],
        ),
        (
            "conn-2 exact match only hits its own connection",
            RateLimitMatch {
                key_hash: "key-1",
                usage_pool_id: Some("pool-1"),
                connection_id: Some("conn-2"),
                model: RateLimitModel::Exact("gpt-4o"),
            },
            &["parity-conn2-exact"],
        ),
        (
            "global dispatch matches both direct key and inherited pool globals",
            RateLimitMatch {
                key_hash: "key-1",
                usage_pool_id: Some("pool-1"),
                connection_id: None,
                model: RateLimitModel::NotApplicable,
            },
            &["parity-global-key", "parity-global-pool"],
        ),
        (
            "global dispatch without a usage pool only matches the direct key global",
            RateLimitMatch {
                key_hash: "key-1",
                usage_pool_id: None,
                connection_id: None,
                model: RateLimitModel::NotApplicable,
            },
            &["parity-global-key"],
        ),
        (
            "global dispatch with an unrelated key only matches the pool global",
            RateLimitMatch {
                key_hash: "key-2",
                usage_pool_id: Some("pool-1"),
                connection_id: None,
                model: RateLimitModel::NotApplicable,
            },
            &["parity-global-pool"],
        ),
    ];

    for (case, request, expected) in scenarios {
        let mut expected: Vec<&str> = expected.to_vec();
        expected.sort_unstable();

        let mut memory_ids: Vec<String> = memory
            .matching(&request)
            .await
            .unwrap()
            .into_iter()
            .map(|policy| policy.id)
            .collect();
        memory_ids.sort_unstable();

        let mut postgres_ids: Vec<String> = postgres
            .matching(&request)
            .await
            .unwrap()
            .into_iter()
            .map(|policy| policy.id)
            .collect();
        postgres_ids.sort_unstable();

        assert_eq!(memory_ids, expected, "{case}: memory store");
        assert_eq!(
            postgres_ids, expected,
            "{case}: postgres store (must agree with memory)"
        );
    }
}

async fn seed_runtime_policies(pool: &sqlx::PgPool, environment: &str, ids: &[String]) {
    for policy_id in ids {
        let budget_id = uuid::Uuid::new_v4().to_string();
        // `scope_id` must vary per policy: the canonical index is unique on
        // (environment_id, scope, scope_id, period), and every caller here
        // shares the same 'api_key'/'daily' scope/period, some of them even
        // under the same literal `environment`. A single hardcoded 'key'
        // scope_id would collide with itself across multiple ids in one call
        // and across calls sharing an environment. Runtime lookups key off
        // `policy_id`, not `scope_id` (see PostgresQuota), so this is safe.
        sqlx::query(
            "WITH budget AS ( \
                INSERT INTO chalk_router.budget_limits \
                    (environment_id, id, scope, scope_id, window_type, window_size, \
                     limit_kind, limit_amount, enabled) \
                VALUES ($1, $2, 'api_key', $4, 'calendar', INTERVAL '1 day', \
                        'tokens', 1000, true) \
             ) \
             INSERT INTO chalk_router.quota_policies \
                (id, environment_id, budget_limit_id) VALUES ($3, $1, $2)",
        )
        .bind(environment)
        .bind(budget_id)
        .bind(policy_id)
        .bind(policy_id)
        .execute(pool)
        .await
        .expect("seed quota policy");
    }
}

#[tokio::test]
async fn postgres_quota_satisfies_conformance() {
    let Some(pool) = postgres_pool().await else {
        eprintln!("skipping PostgreSQL quota conformance; set TEST_DATABASE_URL");
        return;
    };
    let prefix = format!("postgres-{}", uuid::Uuid::new_v4());
    let ids = ["rpm", "budget", "concurrency", "fresh", "blocked"]
        .map(|suffix| format!("{prefix}-{suffix}"));
    seed_runtime_policies(&pool, "quota-conformance", &ids).await;
    run_suite(Arc::new(PostgresQuota::new(pool)), &prefix).await;
}

async fn background_quota_suite(pool: Option<sqlx::PgPool>) {
    for (suffix, outcome, expected) in [
        ("actual", QuotaOutcome::Actual(5), 5),
        ("unknown", QuotaOutcome::Unknown, 80),
        ("failed", QuotaOutcome::Failed, 3),
        ("rollover", QuotaOutcome::Actual(5), 10),
    ] {
        let prefix = format!("background-{suffix}-{}", uuid::Uuid::new_v4());
        let owner = BackgroundOwner {
            environment_id: "quota-conformance".into(),
            event_id: uuid::Uuid::new_v4(),
        };
        let start = at(Utc::now().timestamp() - 60);
        let budget = fixed_action_for_window(
            &format!("{prefix}-budget"),
            80,
            start,
            start + chrono::Duration::days(1),
        );
        let concurrency = action(
            &format!("{prefix}-concurrency"),
            QuotaSource::RateLimit,
            QuotaAlgorithm::Concurrency,
            QuotaMeter::ConcurrentRequests,
            1,
            None,
            1,
        );
        let tokens = action(
            &format!("{prefix}-tpm"),
            QuotaSource::RateLimit,
            QuotaAlgorithm::TokenBucket,
            QuotaMeter::Tokens,
            100,
            Some(100),
            80,
        );
        if let Some(pool) = &pool {
            seed_runtime_policies(
                pool,
                &owner.environment_id,
                &[
                    budget.policy_id.clone(),
                    concurrency.policy_id.clone(),
                    tokens.policy_id.clone(),
                ],
            )
            .await;
            sqlx::query("INSERT INTO chalk_router.background_response_jobs(environment_id,event_id,phase,payload,next_poll_at) VALUES($1,$2,'submitting','{}',now())")
                .bind(&owner.environment_id).bind(owner.event_id).execute(pool).await.unwrap();
        }
        let backend: Arc<dyn QuotaBackend> = pool.as_ref().map_or_else(
            || Arc::new(MemoryQuota::new()) as Arc<dyn QuotaBackend>,
            |pool| Arc::new(PostgresQuota::new(pool.clone())) as Arc<dyn QuotaBackend>,
        );
        let QuotaDecision::Allowed(Some(grant)) = backend
            .acquire_background(
                &[budget.clone(), concurrency.clone(), tokens.clone()],
                Some(3),
                &owner,
            )
            .await
            .unwrap()
        else {
            panic!("expected owned grant");
        };
        drop(openai_server::quota::QuotaPermit::new(
            backend.clone(),
            grant,
        ));
        if let Some(pool) = &pool {
            sqlx::query("UPDATE chalk_router.quota_leases SET expires_at=now()-interval '1 hour' WHERE policy_id=$1")
                .bind(&concurrency.policy_id).execute(pool).await.unwrap();
            sqlx::query("UPDATE chalk_router.quota_reservations SET expires_at=now()-interval '1 hour' WHERE policy_id IN ($1,$2)")
                .bind(&budget.policy_id).bind(&tokens.policy_id).execute(pool).await.unwrap();
        }
        backend.cleanup(1000).await.unwrap();
        let restarted: Arc<dyn QuotaBackend> = pool.as_ref().map_or_else(
            || backend.clone(),
            |pool| Arc::new(PostgresQuota::new(pool.clone())),
        );
        assert!(matches!(
            restarted
                .acquire_many(std::slice::from_ref(&concurrency), None)
                .await
                .unwrap(),
            QuotaDecision::Limited(_)
        ));
        if let Some(pool) = &pool {
            let mut transaction = pool.begin().await.unwrap();
            assert!(restarted
                .finish_background_in_transaction(&mut transaction, &owner, outcome)
                .await
                .unwrap());
            transaction.rollback().await.unwrap();
            assert!(matches!(
                restarted
                    .acquire_many(std::slice::from_ref(&concurrency), None)
                    .await
                    .unwrap(),
                QuotaDecision::Limited(_)
            ));
        }
        let queried_window = if suffix == "rollover" {
            let next_window = start + chrono::Duration::days(1);
            let next_budget = fixed_action_for_window(
                &budget.policy_id,
                10,
                next_window,
                next_window + chrono::Duration::days(1),
            );
            assert!(matches!(
                restarted.acquire_many(&[next_budget], None).await.unwrap(),
                QuotaDecision::Allowed(_)
            ));
            next_window
        } else {
            start
        };
        let (first, second) = tokio::join!(
            restarted.finish_background(&owner, outcome),
            restarted.finish_background(&owner, outcome)
        );
        first.unwrap();
        second.unwrap();
        restarted
            .finish_background(&owner, QuotaOutcome::Actual(99))
            .await
            .unwrap();
        let usage = restarted
            .fixed_window_usage(&[FixedWindowUsageQuery {
                policy_id: budget.policy_id.clone(),
                window_start: queried_window,
            }])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(usage[&budget.policy_id], expected);
        assert!(matches!(
            restarted.acquire_many(&[concurrency], None).await.unwrap(),
            QuotaDecision::Allowed(_)
        ));
        if let Some(pool) = &pool {
            let pending:i64=sqlx::query_scalar("SELECT (SELECT count(*) FROM chalk_router.quota_reservations WHERE background_environment_id=$1 AND background_event_id=$2)+(SELECT count(*) FROM chalk_router.quota_leases WHERE background_environment_id=$1 AND background_event_id=$2)")
                .bind(&owner.environment_id).bind(owner.event_id).fetch_one(pool).await.unwrap();
            assert_eq!(pending, 0);
        }
    }
}

#[tokio::test]
async fn memory_background_quota_satisfies_conformance() {
    background_quota_suite(None).await;
}

#[tokio::test]
async fn postgres_background_quota_satisfies_conformance() {
    let Some(pool) = postgres_pool().await else {
        return;
    };
    background_quota_suite(Some(pool)).await;
}

#[tokio::test]
async fn postgres_fixed_window_late_settlement_does_not_mutate_new_window() {
    let Some(pool) = postgres_pool().await else {
        eprintln!("skipping PostgreSQL fixed-window rollover test; set TEST_DATABASE_URL");
        return;
    };
    let policy_id = format!("postgres-fixed-window-rollover-{}", uuid::Uuid::new_v4());
    seed_runtime_policies(&pool, "quota-conformance", std::slice::from_ref(&policy_id)).await;
    fixed_window_late_settlement_cannot_mutate_new_window(
        Arc::new(PostgresQuota::new(pool)),
        &policy_id,
    )
    .await;
}

#[tokio::test]
async fn postgres_fixed_window_late_excess_carries_into_the_next_window() {
    let Some(pool) = postgres_pool().await else {
        eprintln!("skipping PostgreSQL fixed-window late excess test; set TEST_DATABASE_URL");
        return;
    };
    let suffix = uuid::Uuid::new_v4();
    let ids = [
        format!("postgres-late-excess-rolled-{suffix}"),
        format!("postgres-late-excess-unrolled-{suffix}"),
    ];
    seed_runtime_policies(&pool, "quota-conformance", &ids).await;
    fixed_window_late_excess_carries_into_the_next_window(
        Arc::new(PostgresQuota::new(pool)),
        &ids[0],
        &ids[1],
    )
    .await;
}

#[tokio::test]
async fn postgres_renew_extends_foreground_token_reservations() {
    let Some(pool) = postgres_pool().await else {
        eprintln!("skipping PostgreSQL reservation renewal test; set TEST_DATABASE_URL");
        return;
    };
    let policy_id = format!("postgres-reservation-renewal-{}", uuid::Uuid::new_v4());
    seed_runtime_policies(&pool, "quota-conformance", std::slice::from_ref(&policy_id)).await;
    let backend = PostgresQuota::new(pool.clone());
    let grant = match backend
        .acquire_many(&[fixed_action(&policy_id, 10)], None)
        .await
        .unwrap()
    {
        QuotaDecision::Allowed(Some(grant)) => grant,
        other => panic!("expected a grant, got {other:?}"),
    };
    sqlx::query(
        "UPDATE chalk_router.quota_reservations \
         SET expires_at = clock_timestamp() + INTERVAL '1 second' WHERE permit_id = $1",
    )
    .bind(grant.permit_id)
    .execute(&pool)
    .await
    .unwrap();

    backend.renew(grant.permit_id).await.unwrap();

    let remaining: f64 = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM expires_at - clock_timestamp())::DOUBLE PRECISION \
         FROM chalk_router.quota_reservations WHERE permit_id = $1",
    )
    .bind(grant.permit_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        remaining > 300.0,
        "a running request must keep its reservation: {remaining}s left"
    );
    backend.finish(&grant, QuotaOutcome::Unknown).await.unwrap();
}

async fn background_owned_rows_snapshot_suite(pool: Option<sqlx::PgPool>) {
    for (suffix, outcome, expected) in [
        ("worker", QuotaOutcome::Actual(5), 5),
        ("permit", QuotaOutcome::Failed, 3),
    ] {
        let prefix = format!("owned-snapshot-{suffix}-{}", uuid::Uuid::new_v4());
        let owner = BackgroundOwner {
            environment_id: "quota-conformance".into(),
            event_id: uuid::Uuid::new_v4(),
        };
        let start = at(Utc::now().timestamp() - 60);
        let budget = fixed_action_for_window(
            &format!("{prefix}-budget"),
            80,
            start,
            start + chrono::Duration::days(1),
        );
        let tokens = action(
            &format!("{prefix}-tokens"),
            QuotaSource::RateLimit,
            QuotaAlgorithm::TokenBucket,
            QuotaMeter::Tokens,
            100,
            Some(100),
            80,
        );
        let concurrency = action(
            &format!("{prefix}-concurrency"),
            QuotaSource::RateLimit,
            QuotaAlgorithm::Concurrency,
            QuotaMeter::ConcurrentRequests,
            1,
            None,
            1,
        );
        if let Some(pool) = &pool {
            seed_runtime_policies(
                pool,
                &owner.environment_id,
                &[
                    budget.policy_id.clone(),
                    tokens.policy_id.clone(),
                    concurrency.policy_id.clone(),
                ],
            )
            .await;
            sqlx::query("INSERT INTO chalk_router.background_response_jobs(environment_id,event_id,phase,payload,next_poll_at) VALUES($1,$2,'submitting','{}',now())")
                .bind(&owner.environment_id).bind(owner.event_id).execute(pool).await.unwrap();
        }
        let backend: Arc<dyn QuotaBackend> = pool.as_ref().map_or_else(
            || Arc::new(MemoryQuota::new()) as Arc<dyn QuotaBackend>,
            |pool| Arc::new(PostgresQuota::new(pool.clone())) as Arc<dyn QuotaBackend>,
        );
        let QuotaDecision::Allowed(Some(mut budget_permit)) = backend
            .acquire_background(std::slice::from_ref(&budget), Some(3), &owner)
            .await
            .unwrap()
        else {
            panic!("expected budget reservation");
        };
        let QuotaDecision::Allowed(Some(mut attempt_permit)) = backend
            .acquire_background(&[tokens.clone(), concurrency.clone()], Some(3), &owner)
            .await
            .unwrap()
        else {
            panic!("expected route reservation and lease");
        };
        if let Some(pool) = &pool {
            let stored: (String, i64, Option<i64>, Option<i64>) = sqlx::query_as(
                "SELECT algorithm,limit_value,burst,failed_amount FROM chalk_router.quota_reservations WHERE policy_id=$1 AND background_environment_id=$2 AND background_event_id=$3",
            ).bind(&tokens.policy_id).bind(&owner.environment_id).bind(owner.event_id)
                .fetch_one(pool).await.unwrap();
            assert_eq!(stored, ("token_bucket".into(), 100, Some(100), Some(3)));
            sqlx::query(
                "UPDATE chalk_router.budget_limits AS budget SET limit_amount=1 FROM chalk_router.quota_policies AS policy WHERE policy.environment_id=budget.environment_id AND policy.budget_limit_id=budget.id AND policy.id=ANY($1)",
            )
            .bind(vec![budget.policy_id.clone(), tokens.policy_id.clone()])
            .execute(pool)
            .await
            .unwrap();
        }
        let other_owner = BackgroundOwner {
            event_id: uuid::Uuid::new_v4(),
            ..owner.clone()
        };
        backend
            .finish_background(&other_owner, QuotaOutcome::Actual(0))
            .await
            .unwrap();
        assert!(matches!(
            backend
                .acquire_many(std::slice::from_ref(&concurrency), None)
                .await
                .unwrap(),
            QuotaDecision::Limited(_)
        ));
        for permit in [&mut budget_permit, &mut attempt_permit] {
            permit.failed_amount = Some(77);
            for action in &mut permit.actions {
                action.algorithm = QuotaAlgorithm::TokenBucket;
                action.limit_value = 1;
                action.burst = Some(1);
            }
        }
        let restarted: Arc<dyn QuotaBackend> = pool.as_ref().map_or_else(
            || backend.clone(),
            |pool| Arc::new(PostgresQuota::new(pool.clone())) as Arc<dyn QuotaBackend>,
        );
        if suffix == "permit" {
            restarted.finish(&budget_permit, outcome).await.unwrap();
            restarted.finish(&attempt_permit, outcome).await.unwrap();
        } else {
            restarted.finish_background(&owner, outcome).await.unwrap();
        }
        restarted
            .finish_background(&owner, QuotaOutcome::Actual(99))
            .await
            .unwrap();
        let usage = restarted
            .fixed_window_usage(&[FixedWindowUsageQuery {
                policy_id: budget.policy_id.clone(),
                window_start: start,
            }])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(usage[&budget.policy_id], expected);
        assert!(matches!(
            restarted
                .acquire_many(&[tokens, concurrency], None)
                .await
                .unwrap(),
            QuotaDecision::Allowed(_)
        ));
    }
}

#[tokio::test]
async fn memory_background_owned_rows_preserve_settlement_snapshot() {
    background_owned_rows_snapshot_suite(None).await;
}

#[tokio::test]
async fn postgres_background_owned_rows_preserve_settlement_snapshot() {
    let Some(pool) = postgres_pool().await else {
        return;
    };
    background_owned_rows_snapshot_suite(Some(pool)).await;
}

async fn background_lease_only_suite(pool: Option<sqlx::PgPool>) {
    let owner = BackgroundOwner {
        environment_id: "quota-conformance".into(),
        event_id: uuid::Uuid::new_v4(),
    };
    let concurrency = action(
        &format!("lease-only-{}", uuid::Uuid::new_v4()),
        QuotaSource::RateLimit,
        QuotaAlgorithm::Concurrency,
        QuotaMeter::ConcurrentRequests,
        1,
        None,
        1,
    );
    if let Some(pool) = &pool {
        seed_runtime_policies(
            pool,
            &owner.environment_id,
            std::slice::from_ref(&concurrency.policy_id),
        )
        .await;
        sqlx::query("INSERT INTO chalk_router.background_response_jobs(environment_id,event_id,phase,payload,next_poll_at) VALUES($1,$2,'submitting','{}',now())")
            .bind(&owner.environment_id).bind(owner.event_id).execute(pool).await.unwrap();
    }
    let backend: Arc<dyn QuotaBackend> = pool.as_ref().map_or_else(
        || Arc::new(MemoryQuota::new()) as Arc<dyn QuotaBackend>,
        |pool| Arc::new(PostgresQuota::new(pool.clone())) as Arc<dyn QuotaBackend>,
    );
    let QuotaDecision::Allowed(Some(permit)) = backend
        .acquire_background(std::slice::from_ref(&concurrency), None, &owner)
        .await
        .unwrap()
    else {
        panic!("expected owned concurrency lease");
    };
    drop(openai_server::quota::QuotaPermit::new(
        backend.clone(),
        permit,
    ));
    assert!(matches!(
        backend
            .acquire_many(std::slice::from_ref(&concurrency), None)
            .await
            .unwrap(),
        QuotaDecision::Limited(_)
    ));
    backend
        .finish_background(&owner, QuotaOutcome::Unknown)
        .await
        .unwrap();
    backend
        .finish_background(&owner, QuotaOutcome::Unknown)
        .await
        .unwrap();
    assert!(matches!(
        backend.acquire_many(&[concurrency], None).await.unwrap(),
        QuotaDecision::Allowed(_)
    ));
}

#[tokio::test]
async fn memory_background_lease_only_settlement() {
    background_lease_only_suite(None).await;
}

#[tokio::test]
async fn postgres_background_lease_only_settlement() {
    let Some(pool) = postgres_pool().await else {
        return;
    };
    background_lease_only_suite(Some(pool)).await;
}

#[tokio::test]
async fn postgres_background_owned_rows_enforce_job_foreign_keys() {
    let Some(pool) = postgres_pool().await else {
        return;
    };
    let owner = BackgroundOwner {
        environment_id: "quota-conformance".into(),
        event_id: uuid::Uuid::new_v4(),
    };
    let concurrency = action(
        &format!("ownership-fk-{}", uuid::Uuid::new_v4()),
        QuotaSource::RateLimit,
        QuotaAlgorithm::Concurrency,
        QuotaMeter::ConcurrentRequests,
        1,
        None,
        1,
    );
    seed_runtime_policies(
        &pool,
        &owner.environment_id,
        std::slice::from_ref(&concurrency.policy_id),
    )
    .await;
    let backend = PostgresQuota::new(pool.clone());
    assert!(backend
        .acquire_background(std::slice::from_ref(&concurrency), None, &owner)
        .await
        .is_err());
    for insert in [
        "INSERT INTO chalk_router.quota_leases(policy_id,permit_id,expires_at,background_environment_id,background_event_id) VALUES($1,$2,now(),$3,$4)",
        "INSERT INTO chalk_router.quota_reservations(policy_id,permit_id,reserved_amount,expires_at,background_environment_id,background_event_id,algorithm,limit_value) VALUES($1,$2,80,now(),$3,$4,'fixed_window',100)",
    ] {
        for (environment_id, event_id) in [
            (Some(owner.environment_id.as_str()), None),
            (None, Some(owner.event_id)),
            (Some(owner.environment_id.as_str()), Some(owner.event_id)),
        ] {
            let error = sqlx::query(insert)
                .bind(&concurrency.policy_id).bind(uuid::Uuid::new_v4()).bind(environment_id).bind(event_id)
                .execute(&pool).await.unwrap_err();
            assert_eq!(error.as_database_error().and_then(|error| error.code()).as_deref(), Some("23503"));
        }
    }
    sqlx::query("INSERT INTO chalk_router.background_response_jobs(environment_id,event_id,phase,payload,next_poll_at) VALUES($1,$2,'submitting','{}',now())")
        .bind(&owner.environment_id).bind(owner.event_id).execute(&pool).await.unwrap();
    let QuotaDecision::Allowed(Some(permit)) = backend
        .acquire_background(&[concurrency], None, &owner)
        .await
        .unwrap()
    else {
        panic!("expected owned lease");
    };
    assert!(sqlx::query(
        "DELETE FROM chalk_router.background_response_jobs WHERE environment_id=$1 AND event_id=$2"
    )
    .bind(&owner.environment_id)
    .bind(owner.event_id)
    .execute(&pool)
    .await
    .is_err());
    backend
        .finish(&permit, QuotaOutcome::Unknown)
        .await
        .unwrap();
    sqlx::query(
        "DELETE FROM chalk_router.background_response_jobs WHERE environment_id=$1 AND event_id=$2",
    )
    .bind(&owner.environment_id)
    .bind(owner.event_id)
    .execute(&pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn postgres_bridge_rejects_cross_environment_budget() {
    let Some(pool) = postgres_pool().await else {
        eprintln!("skipping PostgreSQL bridge constraint test; set TEST_DATABASE_URL");
        return;
    };
    let suffix = uuid::Uuid::new_v4().to_string();
    let budget_id = format!("budget-{suffix}");
    sqlx::query(
        "INSERT INTO chalk_router.budget_limits \
            (environment_id, id, scope, scope_id, window_type, window_size, \
             limit_kind, limit_amount, enabled) \
         VALUES ($1, $2, 'api_key', 'key', 'calendar', INTERVAL '1 day', \
                 'tokens', 10, true)",
    )
    .bind(format!("environment-a-{suffix}"))
    .bind(&budget_id)
    .execute(&pool)
    .await
    .unwrap();
    let error = sqlx::query(
        "INSERT INTO chalk_router.quota_policies \
            (id, environment_id, budget_limit_id) VALUES ($1, $2, $3)",
    )
    .bind(format!("policy-{suffix}"))
    .bind(format!("environment-b-{suffix}"))
    .bind(&budget_id)
    .execute(&pool)
    .await
    .expect_err("composite FK must reject cross-environment bridge");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23503")
    );

    let error =
        sqlx::query("INSERT INTO chalk_router.quota_policies (id, environment_id) VALUES ($1, $2)")
            .bind(format!("empty-policy-{suffix}"))
            .bind(format!("environment-a-{suffix}"))
            .execute(&pool)
            .await
            .expect_err("a quota bridge must select exactly one source");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let valid_policy_id = format!("valid-policy-{suffix}");
    sqlx::query(
        "INSERT INTO chalk_router.quota_policies \
            (id, environment_id, budget_limit_id) VALUES ($1, $2, $3)",
    )
    .bind(&valid_policy_id)
    .bind(format!("environment-a-{suffix}"))
    .bind(&budget_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM chalk_router.budget_limits WHERE environment_id = $1 AND id = $2")
        .bind(format!("environment-a-{suffix}"))
        .bind(&budget_id)
        .execute(&pool)
        .await
        .unwrap();
    let remains: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM chalk_router.quota_policies WHERE id = $1)",
    )
    .bind(valid_policy_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!remains, "deleting a source cascades its quota bridge");
}

#[tokio::test]
async fn postgres_janitor_repairs_cold_concurrency_state() {
    let Some(pool) = postgres_pool().await else {
        eprintln!("skipping PostgreSQL janitor test; set TEST_DATABASE_URL");
        return;
    };
    let policy_id = format!("janitor-{}", uuid::Uuid::new_v4());
    seed_runtime_policies(&pool, "quota-conformance", std::slice::from_ref(&policy_id)).await;
    sqlx::query("INSERT INTO chalk_router.quota_states (policy_id, active_count) VALUES ($1, 2)")
        .bind(&policy_id)
        .execute(&pool)
        .await
        .unwrap();
    for _ in 0..2 {
        sqlx::query(
            "INSERT INTO chalk_router.quota_leases (policy_id, permit_id, expires_at) \
             VALUES ($1, $2, now() - INTERVAL '1 second')",
        )
        .bind(&policy_id)
        .bind(uuid::Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
    }
    let backend = PostgresQuota::new(pool.clone());
    let (_, leases) = backend.cleanup(1).await.unwrap();
    assert_eq!(leases, 1);
    let active: i64 = sqlx::query_scalar(
        "SELECT active_count FROM chalk_router.quota_states WHERE policy_id = $1",
    )
    .bind(&policy_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active, 1, "the janitor batch size must bound lease cleanup");
    let (_, leases) = backend.cleanup(1).await.unwrap();
    assert_eq!(leases, 1);
    let active: i64 = sqlx::query_scalar(
        "SELECT active_count FROM chalk_router.quota_states WHERE policy_id = $1",
    )
    .bind(&policy_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active, 0);
}

#[tokio::test]
async fn memory_rejects_duplicate_actions_without_consuming_capacity() {
    let backend = MemoryQuota::new();
    let action = action(
        "duplicate",
        QuotaSource::RateLimit,
        QuotaAlgorithm::TokenBucket,
        QuotaMeter::Requests,
        1,
        Some(1),
        1,
    );
    assert!(backend
        .acquire_many(&[action.clone(), action.clone()], None)
        .await
        .is_err());
    assert!(matches!(
        backend.acquire_many(&[action], None).await.unwrap(),
        QuotaDecision::Allowed(None)
    ));
}

async fn two_phase_admission_suite(backend: Arc<dyn QuotaBackend>, prefix: &str) {
    use openai_server::quota::QuotaAdmission;
    let rpm = action(
        &format!("{prefix}-request-rpm"),
        QuotaSource::RateLimit,
        QuotaAlgorithm::TokenBucket,
        QuotaMeter::Requests,
        1,
        Some(1),
        1,
    );
    let budget = fixed_action(&format!("{prefix}-request-budget"), 80);
    let blocked = action(
        &format!("{prefix}-route-blocked"),
        QuotaSource::RateLimit,
        QuotaAlgorithm::TokenBucket,
        QuotaMeter::Requests,
        1,
        Some(1),
        2,
    );
    let request_actions = [budget.clone(), rpm.clone()];
    let QuotaAdmission::RequestAllowed {
        request_grant: Some(request_grant),
        route: Ok(QuotaDecision::Limited(_)),
    } = backend
        .acquire_request_and_route(&request_actions, std::slice::from_ref(&blocked), Some(3))
        .await
        .unwrap()
    else {
        panic!("request admitted before route denial")
    };
    // Nothing dispatched: refund reconcilable reservations, retaining RPM.
    backend
        .finish(&request_grant, QuotaOutcome::Actual(0))
        .await
        .unwrap();
    assert!(matches!(
        backend
            .acquire_request_and_route(
                &request_actions,
                &[QuotaAction {
                    amount: 1,
                    ..blocked.clone()
                }],
                None,
            )
            .await
            .unwrap(),
        QuotaAdmission::RequestLimited(_)
    ));
    assert!(matches!(
        backend
            .acquire_request_and_route(&request_actions, &[], None)
            .await
            .unwrap(),
        QuotaAdmission::RequestLimited(_)
    ));
    assert!(matches!(
        backend
            .acquire_request_and_route(&[], std::slice::from_ref(&blocked), None)
            .await
            .unwrap(),
        QuotaAdmission::RequestAllowed {
            request_grant: None,
            route: Ok(QuotaDecision::Limited(_))
        }
    ));
    assert!(matches!(
        backend
            .acquire_many(
                &[QuotaAction {
                    amount: 1,
                    ..blocked
                }],
                None
            )
            .await
            .unwrap(),
        QuotaDecision::Allowed(None)
    ));
    assert!(matches!(
        backend
            .acquire_many(
                &[QuotaAction {
                    amount: 100,
                    ..budget
                }],
                None
            )
            .await
            .unwrap(),
        QuotaDecision::Allowed(Some(_))
    ));

    let budget = fixed_action(&format!("{prefix}-success-budget"), 80);
    let tokens = action(
        &format!("{prefix}-success-tokens"),
        QuotaSource::RateLimit,
        QuotaAlgorithm::TokenBucket,
        QuotaMeter::Tokens,
        1,
        Some(20),
        20,
    );
    let concurrency = action(
        &format!("{prefix}-success-concurrency"),
        QuotaSource::RateLimit,
        QuotaAlgorithm::Concurrency,
        QuotaMeter::ConcurrentRequests,
        1,
        None,
        1,
    );
    let QuotaAdmission::RequestAllowed {
        request_grant: Some(request_grant),
        route: Ok(QuotaDecision::Allowed(Some(route_grant))),
    } = backend
        .acquire_request_and_route(
            std::slice::from_ref(&budget),
            &[tokens.clone(), concurrency.clone()],
            Some(3),
        )
        .await
        .unwrap()
    else {
        panic!("both phases admitted")
    };
    assert_ne!(request_grant.permit_id, route_grant.permit_id);
    assert_eq!(request_grant.failed_amount, None);
    assert_eq!(route_grant.failed_amount, Some(3));
    backend
        .finish(&route_grant, QuotaOutcome::Failed)
        .await
        .unwrap();
    backend
        .finish(&request_grant, QuotaOutcome::Actual(10))
        .await
        .unwrap();
    assert!(matches!(
        backend
            .acquire_many(
                &[QuotaAction {
                    amount: 91,
                    ..budget
                }],
                None
            )
            .await
            .unwrap(),
        QuotaDecision::Limited(_)
    ));
    assert!(matches!(
        backend
            .acquire_many(
                &[QuotaAction {
                    amount: 18,
                    ..tokens.clone()
                }],
                None
            )
            .await
            .unwrap(),
        QuotaDecision::Limited(_)
    ));
    assert!(matches!(
        backend
            .acquire_many(
                &[QuotaAction {
                    amount: 17,
                    ..tokens
                }],
                None
            )
            .await
            .unwrap(),
        QuotaDecision::Allowed(Some(_))
    ));
    assert!(matches!(
        backend.acquire_many(&[concurrency], None).await.unwrap(),
        QuotaDecision::Allowed(Some(_))
    ));
    assert!(matches!(
        backend
            .acquire_request_and_route(&[], &[], None)
            .await
            .unwrap(),
        QuotaAdmission::RequestAllowed {
            request_grant: None,
            route: Ok(QuotaDecision::Allowed(None))
        }
    ));
}

#[tokio::test]
async fn memory_two_phase_admission_preserves_charges_and_grants() {
    two_phase_admission_suite(Arc::new(MemoryQuota::new()), "memory-two-phase").await;
}

#[tokio::test]
async fn postgres_two_phase_admission_preserves_charges_and_grants() {
    let Some(pool) = postgres_pool().await else {
        return;
    };
    let prefix = format!("two-phase-{}", uuid::Uuid::new_v4());
    let ids = [
        "request-rpm",
        "request-budget",
        "route-blocked",
        "success-budget",
        "success-tokens",
        "success-concurrency",
    ]
    .map(|suffix| format!("{prefix}-{suffix}"));
    seed_runtime_policies(&pool, "quota-conformance", &ids).await;
    two_phase_admission_suite(Arc::new(PostgresQuota::new(pool)), &prefix).await;
}

#[tokio::test]
async fn postgres_bucket_admission_writes_once_and_denial_persists_refill() {
    let Some(pool) = postgres_pool().await else {
        return;
    };
    let prefix = format!("bucket-write-{}", uuid::Uuid::new_v4());
    let id = format!("{prefix}-bucket");
    seed_runtime_policies(&pool, "quota-conformance", std::slice::from_ref(&id)).await;
    let bucket = action(
        &id,
        QuotaSource::RateLimit,
        QuotaAlgorithm::TokenBucket,
        QuotaMeter::Requests,
        60,
        Some(10),
        10,
    );
    let mut transaction = pool.begin().await.unwrap();
    let before: i64 = sqlx::query_scalar("SELECT n_tup_upd FROM pg_stat_xact_user_tables WHERE schemaname='chalk_router' AND relname='quota_states'").fetch_one(&mut *transaction).await.unwrap();
    let allowed: bool =
        sqlx::query_scalar("SELECT allowed FROM chalk_router.acquire_quota($1,$2,60,300)")
            .bind(sqlx::types::Json([bucket.clone()]))
            .bind(uuid::Uuid::new_v4())
            .fetch_one(&mut *transaction)
            .await
            .unwrap();
    assert!(allowed);
    let after: i64 = sqlx::query_scalar("SELECT n_tup_upd FROM pg_stat_xact_user_tables WHERE schemaname='chalk_router' AND relname='quota_states'").fetch_one(&mut *transaction).await.unwrap();
    assert_eq!(
        after - before,
        1,
        "refill and deduction are a single state write"
    );
    sqlx::query("UPDATE chalk_router.quota_states SET balance=0, updated_at=clock_timestamp()-INTERVAL '2 seconds' WHERE policy_id=$1").bind(&id).execute(&mut *transaction).await.unwrap();
    let allowed: bool =
        sqlx::query_scalar("SELECT allowed FROM chalk_router.acquire_quota($1,$2,60,300)")
            .bind(sqlx::types::Json([QuotaAction {
                amount: 11,
                ..bucket
            }]))
            .bind(uuid::Uuid::new_v4())
            .fetch_one(&mut *transaction)
            .await
            .unwrap();
    assert!(!allowed);
    let refilled: f64 = sqlx::query_scalar(
        "SELECT balance::DOUBLE PRECISION FROM chalk_router.quota_states WHERE policy_id=$1",
    )
    .bind(&id)
    .fetch_one(&mut *transaction)
    .await
    .unwrap();
    assert!(
        (2.0..=10.0).contains(&refilled),
        "denial persisted refill without deducting: {refilled}"
    );
    transaction.rollback().await.unwrap();
}

async fn invalid_route_keeps_request_charge(backend: &dyn QuotaBackend, ids: &[String]) {
    use openai_server::quota::QuotaAdmission;
    let rpm = action(
        &ids[0],
        QuotaSource::RateLimit,
        QuotaAlgorithm::TokenBucket,
        QuotaMeter::Requests,
        1,
        Some(1),
        1,
    );
    let budget = fixed_action(&ids[1], 80);
    let invalid_route = action(
        &ids[2],
        QuotaSource::RateLimit,
        QuotaAlgorithm::TokenBucket,
        QuotaMeter::Requests,
        1,
        Some(1),
        1,
    );
    let route = [invalid_route.clone(), invalid_route];
    let QuotaAdmission::RequestAllowed {
        request_grant: Some(grant),
        route: Err(_),
    } = backend
        .acquire_request_and_route(&[rpm.clone(), budget.clone()], &route, None)
        .await
        .unwrap()
    else {
        panic!("invalid route must retain request admission and grant")
    };
    backend
        .finish(&grant, QuotaOutcome::Actual(0))
        .await
        .unwrap();
    assert!(matches!(
        backend.acquire_many(&[rpm.clone()], None).await.unwrap(),
        QuotaDecision::Limited(_)
    ));
    assert!(matches!(
        backend
            .acquire_many(
                &[QuotaAction {
                    amount: 100,
                    ..budget
                }],
                None
            )
            .await
            .unwrap(),
        QuotaDecision::Allowed(Some(_))
    ));
    assert!(
        matches!(
            backend
                .acquire_request_and_route(&[rpm], &route, None)
                .await
                .unwrap(),
            QuotaAdmission::RequestLimited(_)
        ),
        "request denial wins over an invalid route"
    );
}

#[tokio::test]
async fn memory_invalid_route_preserves_request_charge() {
    let ids = [
        "invalid-request-rpm",
        "invalid-request-budget",
        "invalid-route",
    ]
    .map(str::to_string);
    invalid_route_keeps_request_charge(&MemoryQuota::new(), &ids).await;
}

#[tokio::test]
async fn postgres_invalid_route_preserves_request_charge() {
    let Some(pool) = postgres_pool().await else {
        return;
    };
    let unique = uuid::Uuid::new_v4();
    let ids = ["rpm", "budget", "route"].map(|suffix| format!("invalid-{unique}-{suffix}"));
    seed_runtime_policies(&pool, "quota-conformance", &ids).await;
    invalid_route_keeps_request_charge(&PostgresQuota::new(pool), &ids).await;
}

#[tokio::test]
async fn memory_two_phase_route_error_keeps_request_grant_for_cleanup() {
    use openai_server::quota::QuotaAdmission;
    let backend = MemoryQuota::new();
    let budget = fixed_action("two-phase-route-error-budget", 80);
    let invalid_route = action(
        "two-phase-route-error-duplicate",
        QuotaSource::RateLimit,
        QuotaAlgorithm::TokenBucket,
        QuotaMeter::Requests,
        1,
        Some(1),
        1,
    );
    let QuotaAdmission::RequestAllowed {
        request_grant: Some(grant),
        route: Err(_),
    } = backend
        .acquire_request_and_route(
            std::slice::from_ref(&budget),
            &[invalid_route.clone(), invalid_route],
            None,
        )
        .await
        .unwrap()
    else {
        panic!("request grant must survive route-backend failure")
    };
    backend
        .finish(&grant, QuotaOutcome::Actual(0))
        .await
        .unwrap();
    assert!(matches!(
        backend
            .acquire_many(
                &[QuotaAction {
                    amount: 100,
                    ..budget
                }],
                None
            )
            .await
            .unwrap(),
        QuotaDecision::Allowed(Some(_))
    ));
}

#[tokio::test]
async fn postgres_two_phase_route_sql_error_preserves_request_and_rolls_back_route() {
    use openai_server::quota::QuotaAdmission;
    let Some(pool) = postgres_pool().await else {
        return;
    };
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let prefix = format!("route-error-{unique}");
    let ids = ["rpm", "budget", "route"].map(|suffix| format!("{prefix}-{suffix}"));
    seed_runtime_policies(&pool, "quota-conformance", &ids).await;
    let rpm = action(
        &ids[0],
        QuotaSource::RateLimit,
        QuotaAlgorithm::TokenBucket,
        QuotaMeter::Requests,
        1,
        Some(1),
        1,
    );
    let budget = fixed_action(&ids[1], 80);
    let route = action(
        &ids[2],
        QuotaSource::RateLimit,
        QuotaAlgorithm::TokenBucket,
        QuotaMeter::Tokens,
        1,
        Some(20),
        20,
    );
    // SQL identifiers and trigger argument contain only fixed text and a locally
    // generated hexadecimal UUID; no caller data enters this test DDL.
    let trigger = format!("admission_failure_{unique}");
    let function = format!("chalk_router.{trigger}");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE FUNCTION {function}() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN IF NEW.policy_id=TG_ARGV[0] THEN RAISE EXCEPTION 'injected route backend failure'; END IF; RETURN NEW; END; $$")))
        .execute(&pool).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE TRIGGER {trigger} AFTER UPDATE ON chalk_router.quota_states FOR EACH ROW EXECUTE FUNCTION {function}('{}')", ids[2])))
        .execute(&pool).await.unwrap();
    let backend = PostgresQuota::new(pool.clone());
    let decision = backend
        .acquire_request_and_route(&[rpm.clone(), budget], &[route], Some(3))
        .await;
    // Remove the injection before asserting so an unexpected result cannot
    // poison subsequent tests sharing this disposable database.
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
    let QuotaAdmission::RequestAllowed {
        request_grant: Some(grant),
        route: Err(error),
    } = decision.unwrap()
    else {
        panic!("request grant must survive definite route SQL failure")
    };
    assert!(error.to_string().contains("P0001"));
    let used: i64 =
        sqlx::query_scalar("SELECT used_amount FROM chalk_router.quota_states WHERE policy_id=$1")
            .bind(&ids[1])
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(used, 80, "request phase committed");
    let balance: f64 = sqlx::query_scalar(
        "SELECT balance::DOUBLE PRECISION FROM chalk_router.quota_states WHERE policy_id=$1",
    )
    .bind(&ids[2])
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(balance, 20.0, "route phase rolled back");
    assert!(
        matches!(
            backend.acquire_many(&[rpm], None).await.unwrap(),
            QuotaDecision::Limited(_)
        ),
        "request RPM remains charged"
    );
    backend
        .finish(&grant, QuotaOutcome::Actual(0))
        .await
        .unwrap();
    let used: i64 =
        sqlx::query_scalar("SELECT used_amount FROM chalk_router.quota_states WHERE policy_id=$1")
            .bind(&ids[1])
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(used, 0, "retained grant remains reconcilable");
}
