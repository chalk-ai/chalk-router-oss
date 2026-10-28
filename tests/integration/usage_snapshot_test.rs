//! Optional PostgreSQL coverage for API-key usage snapshots. The usage ledger
//! is PostgreSQL-backed even when key metadata itself uses the in-memory store.

use std::collections::HashMap;
use std::sync::Arc;

use openai_server::api_keys::{ApiKeyStore, CreatedApiKey, InMemoryApiKeyStore, UsageSnapshot};
use openai_server::budget::{
    BudgetPolicy, BudgetPolicyStore, MemoryBudgetPolicyStore, PostgresBudgetPolicyStore,
};
use openai_server::hashing::KeyHasher;
use openai_server::service::api_keys::{ApiKeyService, CreateKeyInput};
use openai_server::service::usage::{QueryUsageInput, UsageGrouping, UsageService};
use openai_server::usage_tracker::{UsageFilters, UsageRecord, UsageSource, UsageTracker};

/// Connect a disposable-database pool for one usage-snapshot test, after
/// applying router migrations. Returns `None` (with a skip message printed)
/// when `TEST_DATABASE_URL` is unset, matching every other Postgres-gated
/// test in this file.
async fn connect_usage_test_db(skip_message: &str) -> Option<sqlx::PgPool> {
    let database_url = std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|value| !value.is_empty())?;
    openai_server::migrate::run(&database_url)
        .await
        .expect("apply router migrations");
    let connect_options = openai_server::database::connect_options(&database_url)
        .expect("parse disposable PostgreSQL test database URL");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_with(connect_options)
        .await
        .expect("connect to disposable PostgreSQL test database");
    eprintln!("{skip_message}");
    Some(pool)
}

/// Shared fixture for the key-usage-snapshot tests below: two API keys with
/// buffered `UsageTracker` writes (one of them retried under the same
/// `event_id`, which must not double count), an unused third key, and a row
/// of historical usage recorded directly via SQL — so a snapshot read must
/// combine both the pre-existing and the buffered sources. A read of this
/// fixture's data must reflect `expected_first`/`expected_second` below.
struct KeyUsageFixtures {
    tracker: UsageTracker,
    store: Arc<dyn ApiKeyStore>,
    service: ApiKeyService,
    first: CreatedApiKey,
    second: CreatedApiKey,
    unused: CreatedApiKey,
    first_event_id: uuid::Uuid,
}

fn expected_first_usage() -> UsageSnapshot {
    UsageSnapshot {
        prompt_tokens: 13,
        completion_tokens: 24,
        total_tokens: 37,
        request_count: 3,
    }
}

fn expected_second_usage() -> UsageSnapshot {
    UsageSnapshot {
        prompt_tokens: 5,
        completion_tokens: 6,
        total_tokens: 11,
        request_count: 1,
    }
}

async fn seed_key_usage_fixtures(
    pool: &sqlx::PgPool,
    environment_id: &str,
    other_environment_id: &str,
) -> KeyUsageFixtures {
    let tracker = UsageTracker::new(pool.clone(), environment_id.to_string())
        .await
        .expect("create usage tracker");
    let store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
        b"usage-snapshot-test-pepper".to_vec(),
    )));
    let service = ApiKeyService::new(
        store.clone(),
        Some(tracker.clone()),
        Arc::new(MemoryBudgetPolicyStore::new(environment_id.to_string())),
        Arc::new(openai_server::ratelimit::MemoryRateLimitPolicyStore::new(
            environment_id.to_string(),
        )),
        std::sync::Arc::new(
            openai_server::provider_connections::InMemoryProviderConnectionStore::new(),
        ),
        std::sync::Arc::new(openai_server::model_routes::InMemoryModelRouteStore::new()),
        false,
    );

    let first = service
        .create(CreateKeyInput {
            description: Some("first".into()),
            labels: Some(HashMap::from([
                ("feature".to_string(), "chat".to_string()),
                ("team".to_string(), "alpha".to_string()),
            ])),
            ..Default::default()
        })
        .await
        .expect("create first key");
    let second = service
        .create(CreateKeyInput {
            description: Some("second".into()),
            labels: Some(HashMap::from([
                ("feature".to_string(), "batch".to_string()),
                ("team".to_string(), "beta".to_string()),
            ])),
            ..Default::default()
        })
        .await
        .expect("create second key");
    let unused = service
        .create(CreateKeyInput {
            description: Some("unused".into()),
            ..Default::default()
        })
        .await
        .expect("create unused key");

    sqlx::query(
        "INSERT INTO chalk_router.usage_events \
            (time, environment_id, api_key_hash, usage_pool_id, provider, connection_id, model, \
             prompt_tokens, completion_tokens, cached_tokens, cache_write_tokens, reasoning_tokens, \
             total_tokens, request_count) \
         VALUES \
            (now(), $1, $2, NULL, 'openai', 'db-connection-1', 'model-a', 4, 7, 1, 0, 2, 11, 1), \
            (now(), $1, $2, NULL, 'anthropic', 'db-connection-2', 'model-b', 6, 13, 0, 1, 3, 19, 1), \
            (now(), $3, $2, NULL, 'openai', 'other-environment', 'model-a', 100, 200, 0, 0, 0, 300, 10)",
    )
    .bind(environment_id)
    .bind(&first.api_key.key_hash)
    .bind(other_environment_id)
    .execute(pool)
    .await
    .expect("insert persistent usage fixtures");

    let first_event_id = uuid::Uuid::new_v4();
    tracker
        .record(UsageRecord {
            event_id: first_event_id,
            time: chrono::Utc::now(),
            key_hash: &first.api_key.key_hash,
            usage_pool_id: None,
            connection_id: "buffered-connection",
            provider_kind: "openai",
            model: "model-c",
            prompt_tokens: 3,
            completion_tokens: 4,
            cached_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            usage_source: UsageSource::Estimated,
            input_token_attribution: Default::default(),
        })
        .await
        .expect("persist first call");
    tracker
        .record(UsageRecord {
            event_id: uuid::Uuid::new_v4(),
            time: chrono::Utc::now(),
            key_hash: &second.api_key.key_hash,
            usage_pool_id: None,
            connection_id: "buffered-connection",
            provider_kind: "openai",
            model: "model-a",
            prompt_tokens: 5,
            completion_tokens: 6,
            cached_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            usage_source: UsageSource::Reported,
            input_token_attribution: Default::default(),
        })
        .await
        .expect("persist second call");

    // An ambiguous retry retains its event id and cannot double count.
    tracker
        .record(UsageRecord {
            event_id: first_event_id,
            time: chrono::Utc::now(),
            key_hash: &first.api_key.key_hash,
            usage_pool_id: None,
            connection_id: "buffered-connection",
            provider_kind: "openai",
            model: "model-c",
            prompt_tokens: 3,
            completion_tokens: 4,
            cached_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            usage_source: UsageSource::Estimated,
            input_token_attribution: Default::default(),
        })
        .await
        .expect("idempotent retry");

    KeyUsageFixtures {
        tracker,
        store,
        service,
        first,
        second,
        unused,
        first_event_id,
    }
}

async fn cleanup_key_usage_fixtures(
    pool: &sqlx::PgPool,
    fixtures: KeyUsageFixtures,
    environment_id: &str,
    other_environment_id: &str,
) {
    fixtures.tracker.shutdown().await;
    sqlx::query("DELETE FROM chalk_router.usage_events WHERE environment_id = ANY($1)")
        .bind([environment_id.to_string(), other_environment_id.to_string()])
        .execute(pool)
        .await
        .expect("clean up usage fixtures");
}

#[tokio::test]
async fn key_usage_retry_with_same_event_id_does_not_double_count() {
    let Some(pool) =
        connect_usage_test_db("skipping API-key usage snapshot test; set TEST_DATABASE_URL").await
    else {
        return;
    };
    let environment_id = format!("usage-snapshot-test-{}", uuid::Uuid::new_v4());
    let other_environment_id = format!("{environment_id}-other");
    let fixtures = seed_key_usage_fixtures(&pool, &environment_id, &other_environment_id).await;

    let (provider, usage_source, total_tokens, request_count, call_count): (
        Option<String>,
        i16,
        i64,
        i64,
        i64,
    ) = sqlx::query_as(
        "SELECT provider, usage_source, total_tokens, request_count, \
                (SELECT count(*) FROM chalk_router.usage_events \
                  WHERE environment_id = $1 AND event_id = $3) \
         FROM chalk_router.usage_events \
         WHERE environment_id = $1 AND connection_id = 'buffered-connection' \
           AND api_key_hash = $2 LIMIT 1",
    )
    .bind(&environment_id)
    .bind(&fixtures.first.api_key.key_hash)
    .bind(fixtures.first_event_id)
    .fetch_one(&pool)
    .await
    .expect("read persisted request attribution");
    assert_eq!(provider, None);
    assert_eq!(usage_source, UsageSource::Estimated.as_db_code());
    assert_eq!(total_tokens, 7);
    assert_eq!(request_count, 1);
    assert_eq!(call_count, 1, "the retried event id must produce one row");

    cleanup_key_usage_fixtures(&pool, fixtures, &environment_id, &other_environment_id).await;
}

#[tokio::test]
async fn key_usage_get_usage_combines_historical_and_buffered_totals() {
    let Some(pool) =
        connect_usage_test_db("skipping API-key usage snapshot test; set TEST_DATABASE_URL").await
    else {
        return;
    };
    let environment_id = format!("usage-snapshot-test-{}", uuid::Uuid::new_v4());
    let other_environment_id = format!("{environment_id}-other");
    let fixtures = seed_key_usage_fixtures(&pool, &environment_id, &other_environment_id).await;

    let detail = fixtures
        .service
        .get_usage(&fixtures.first.api_key.key_hash)
        .await
        .expect("get one key's usage");
    assert_eq!(detail.usage, expected_first_usage());

    cleanup_key_usage_fixtures(&pool, fixtures, &environment_id, &other_environment_id).await;
}

#[tokio::test]
async fn key_usage_list_usage_defaults_unused_keys_to_zero() {
    let Some(pool) =
        connect_usage_test_db("skipping API-key usage snapshot test; set TEST_DATABASE_URL").await
    else {
        return;
    };
    let environment_id = format!("usage-snapshot-test-{}", uuid::Uuid::new_v4());
    let other_environment_id = format!("{environment_id}-other");
    let fixtures = seed_key_usage_fixtures(&pool, &environment_id, &other_environment_id).await;

    let all_usage: HashMap<_, _> = fixtures
        .service
        .list_usage()
        .await
        .expect("list all usage")
        .into_iter()
        .map(|entry| (entry.key, entry.usage))
        .collect();
    assert_eq!(
        all_usage[&fixtures.first.api_key.key_hash],
        expected_first_usage()
    );
    assert_eq!(
        all_usage[&fixtures.second.api_key.key_hash],
        expected_second_usage()
    );
    assert_eq!(
        all_usage[&fixtures.unused.api_key.key_hash],
        UsageSnapshot::default()
    );

    cleanup_key_usage_fixtures(&pool, fixtures, &environment_id, &other_environment_id).await;
}

#[tokio::test]
async fn key_usage_list_page_with_usage_matches_list_usage() {
    let Some(pool) =
        connect_usage_test_db("skipping API-key usage snapshot test; set TEST_DATABASE_URL").await
    else {
        return;
    };
    let environment_id = format!("usage-snapshot-test-{}", uuid::Uuid::new_v4());
    let other_environment_id = format!("{environment_id}-other");
    let fixtures = seed_key_usage_fixtures(&pool, &environment_id, &other_environment_id).await;

    let (listed, next_cursor) = fixtures
        .service
        .list_page_with_usage(Some(10), None)
        .await
        .expect("list keys with usage");
    assert!(next_cursor.is_none());
    let listed_usage: HashMap<_, _> = listed
        .into_iter()
        .map(|entry| (entry.api_key.key_hash.clone(), entry.usage))
        .collect();
    assert_eq!(
        listed_usage[&fixtures.first.api_key.key_hash],
        expected_first_usage()
    );
    assert_eq!(
        listed_usage[&fixtures.second.api_key.key_hash],
        expected_second_usage()
    );
    assert_eq!(
        listed_usage[&fixtures.unused.api_key.key_hash],
        UsageSnapshot::default()
    );

    cleanup_key_usage_fixtures(&pool, fixtures, &environment_id, &other_environment_id).await;
}

#[tokio::test]
async fn key_usage_query_grouped_by_api_key_label_filters_by_label_match() {
    let Some(pool) =
        connect_usage_test_db("skipping API-key usage snapshot test; set TEST_DATABASE_URL").await
    else {
        return;
    };
    let environment_id = format!("usage-snapshot-test-{}", uuid::Uuid::new_v4());
    let other_environment_id = format!("{environment_id}-other");
    let fixtures = seed_key_usage_fixtures(&pool, &environment_id, &other_environment_id).await;

    let query_start = chrono::Utc::now() - chrono::Duration::hours(1);
    let query_end = chrono::Utc::now() + chrono::Duration::hours(1);
    let usage_service = UsageService::new(Some(fixtures.tracker.clone()), fixtures.store.clone());
    let rows = usage_service
        .query(QueryUsageInput {
            start: query_start,
            end: query_end,
            bucket_size: "1h".to_string(),
            grouping: UsageGrouping::ApiKeyLabel("feature".to_string()),
            filters: UsageFilters::default(),
            api_key_labels: HashMap::from([("team".to_string(), "alpha".to_string())]),
        })
        .await
        .expect("query usage aggregates");
    assert_eq!(rows.len(), 1, "the team:beta key must be filtered out");
    assert_eq!(rows[0].dimension_value, "chat");
    assert_eq!(rows[0].total_tokens, 37);
    assert_eq!(rows[0].request_count, 3);

    cleanup_key_usage_fixtures(&pool, fixtures, &environment_id, &other_environment_id).await;
}

#[tokio::test]
async fn key_usage_query_grouped_globally_sums_across_all_keys() {
    let Some(pool) =
        connect_usage_test_db("skipping API-key usage snapshot test; set TEST_DATABASE_URL").await
    else {
        return;
    };
    let environment_id = format!("usage-snapshot-test-{}", uuid::Uuid::new_v4());
    let other_environment_id = format!("{environment_id}-other");
    let fixtures = seed_key_usage_fixtures(&pool, &environment_id, &other_environment_id).await;

    let query_start = chrono::Utc::now() - chrono::Duration::hours(1);
    let query_end = chrono::Utc::now() + chrono::Duration::hours(1);
    let usage_service = UsageService::new(Some(fixtures.tracker.clone()), fixtures.store.clone());
    let global_rows = usage_service
        .query(QueryUsageInput {
            start: query_start,
            end: query_end,
            bucket_size: "1h".to_string(),
            grouping: UsageGrouping::Global,
            filters: UsageFilters::default(),
            api_key_labels: HashMap::new(),
        })
        .await
        .expect("query global aggregates");
    assert_eq!(global_rows.len(), 1);
    assert_eq!(global_rows[0].dimension_value, "");
    assert_eq!(
        global_rows[0].total_tokens, 48,
        "must sum first (37) + second (11) keys"
    );
    assert_eq!(global_rows[0].request_count, 4);

    cleanup_key_usage_fixtures(&pool, fixtures, &environment_id, &other_environment_id).await;
}

/// `set_key_daily_budget`/`get_key_daily_budget` take a bare `key_hash` with
/// no foreign key to a real API-key row (see `MemoryBudgetPolicyStore`'s
/// matching impl in `src/budget.rs`, and the `budget_limits` schema), so this
/// round trip needs no API-key fixture of its own.
#[tokio::test]
async fn postgres_budget_set_key_daily_budget_round_trips() {
    let Some(pool) =
        connect_usage_test_db("skipping PostgreSQL budget round-trip test; set TEST_DATABASE_URL")
            .await
    else {
        return;
    };
    let environment_id = format!("usage-snapshot-budget-test-{}", uuid::Uuid::new_v4());
    let key_hash = format!("fake-key-hash-{}", uuid::Uuid::new_v4());
    let budget = PostgresBudgetPolicyStore::new(pool.clone(), environment_id.clone());

    budget
        .set_key_daily_budget(&key_hash, Some(37))
        .await
        .expect("create token budget");
    assert_eq!(
        budget
            .get_key_daily_budget(&key_hash)
            .await
            .expect("read token budget"),
        Some(37)
    );

    sqlx::query("DELETE FROM chalk_router.budget_limits WHERE environment_id = $1")
        .bind(&environment_id)
        .execute(&pool)
        .await
        .expect("clean up budget fixtures");
}

#[tokio::test]
async fn postgres_budget_enable_returns_its_quota_policy_relationship() {
    let database_url = std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|value| !value.is_empty());
    let Some(database_url) = database_url else {
        eprintln!("skipping PostgreSQL budget update test; set TEST_DATABASE_URL");
        return;
    };

    openai_server::migrate::run(&database_url)
        .await
        .expect("apply router migrations");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(
            openai_server::database::connect_options(&database_url)
                .expect("parse disposable PostgreSQL test database URL"),
        )
        .await
        .expect("connect to disposable PostgreSQL test database");

    let suffix = uuid::Uuid::new_v4().to_string();
    let environment_id = format!("budget-update-test-{suffix}");
    let budget_id = format!("budget-{suffix}");
    let quota_policy_id = format!("quota-{suffix}");
    let store = PostgresBudgetPolicyStore::new(pool, environment_id.clone());
    store
        .create(BudgetPolicy {
            quota_policy_id: quota_policy_id.clone(),
            id: budget_id.clone(),
            environment_id,
            scope: "usage_pool".to_string(),
            scope_id: format!("pool-{suffix}"),
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
            limit_amount: 100,
            enabled: false,
            created_at: chrono::Utc::now(),
        })
        .await
        .expect("create disabled usage budget");

    let updated = store
        .update(&budget_id, None, Some(true))
        .await
        .expect("enable usage budget");
    assert_eq!(updated.id, budget_id);
    assert_eq!(updated.quota_policy_id, quota_policy_id);
    assert_eq!(updated.limit_amount, 100);
    assert!(updated.enabled);

    store
        .delete(&updated.id)
        .await
        .expect("clean up usage budget");
}
