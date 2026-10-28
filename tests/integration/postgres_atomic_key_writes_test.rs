//! Optional PostgreSQL coverage for SQL that cannot be exercised by the
//! in-memory stores. Point `TEST_DATABASE_URL` at a disposable database to run
//! it; the default test suite skips without that explicit opt-in.
//!
//! Covers the co-located writes on `PostgresApiKeyStore` (create, budget
//! replace/clear, revoke): each step must keep the key's `rate_limits`,
//! `budget_limits`, and their `quota_policies` bridge rows atomically
//! consistent. This does not assert anything about timing or statement
//! counts, despite the file's former name.

use std::collections::HashMap;

use openai_server::api_keys::{ApiKeyPermission, ApiKeyStore, PostgresApiKeyStore};
use openai_server::hashing::KeyHasher;
use openai_server::ratelimit::ApiKeyRateLimitInput;

#[tokio::test]
async fn atomic_key_writes_keep_rate_limit_and_quota_bridge_rows_consistent() {
    let database_url = std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|value| !value.is_empty());
    let Some(database_url) = database_url else {
        eprintln!("skipping PostgreSQL RPC test; set TEST_DATABASE_URL");
        return;
    };

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

    let environment_id = format!("rpc-perf-test-{}", uuid::Uuid::new_v4());
    let store = PostgresApiKeyStore::new(
        pool.clone(),
        KeyHasher::new(b"postgres-rpc-performance-test".to_vec()),
        environment_id.clone(),
    );
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
    let created = store
        .create_with_quota_definitions(
            "atomic write test".to_string(),
            ApiKeyPermission::default(),
            HashMap::from([
                ("team".to_string(), "alpha".to_string()),
                ("feature".to_string(), "chat".to_string()),
            ]),
            None,
            Some(100),
            vec![ApiKeyRateLimitInput {
                connection_id: Some("conn-1".to_string()),
                models: vec!["gpt-4o".to_string(), "gpt-4.1".to_string()],
                requests_per_minute: Some(60),
                request_burst: Some(10),
                tokens_per_minute: Some(100_000),
                token_burst: Some(20_000),
                concurrent_requests: Some(5),
                enabled: false,
            }],
            None,
        )
        .await
        .expect("atomic key creation succeeds")
        .expect("Postgres store supports co-located budget writes");
    let key_hash = created.api_key.key_hash.clone();
    let (rate_limit_count, disabled_rate_limit_count): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE NOT enabled) FROM chalk_router.rate_limits \
         WHERE environment_id = $1 AND scope = 'api_key' AND scope_id = $2",
    )
    .bind(&environment_id)
    .bind(&key_hash)
    .fetch_one(&pool)
    .await
    .expect("count atomically inserted rate limits");
    assert_eq!(rate_limit_count, 3);
    assert_eq!(disabled_rate_limit_count, 3);
    let (rate_bridges, budget_bridges): (i64, i64) = sqlx::query_as(
        "SELECT \
            (SELECT count(*) FROM chalk_router.quota_policies \
              WHERE environment_id = $1 AND rate_limit_id IS NOT NULL), \
            (SELECT count(*) FROM chalk_router.quota_policies \
              WHERE environment_id = $1 AND budget_limit_id IS NOT NULL)",
    )
    .bind(&environment_id)
    .fetch_one(&pool)
    .await
    .expect("count atomically inserted quota definition rows");
    assert_eq!((rate_bridges, budget_bridges), (3, 1));
    let selected = store
        .select_by_labels(
            &HashMap::from([("team".to_string(), "alpha".to_string())]),
            Some("feature"),
            &[],
            2,
        )
        .await
        .expect("select immutable labels without a GIN index");
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].key_hash, key_hash);

    store
        .update_with_daily_budget(&key_hash, None, None, None, Some(Some(200)))
        .await
        .expect("atomic budget replacement succeeds")
        .expect("Postgres store supports co-located budget writes")
        .expect("created key still exists");

    let (count, amount): (i64, i64) = sqlx::query_as(
        "SELECT count(*), max(limit_amount) \
         FROM chalk_router.budget_limits \
         WHERE environment_id = $1 AND scope = 'api_key' AND scope_id = $2 \
           AND limit_kind = 'tokens' AND window_size = INTERVAL '1 day'",
    )
    .bind(&environment_id)
    .bind(&key_hash)
    .fetch_one(&pool)
    .await
    .expect("query resulting daily budget");

    assert_eq!(count, 1, "sequential replacement leaves one daily budget");
    assert_eq!(amount, 200);
    let bridge_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM chalk_router.quota_policies \
         WHERE environment_id = $1 AND budget_limit_id IS NOT NULL",
    )
    .bind(&environment_id)
    .fetch_one(&pool)
    .await
    .expect("count replacement budget bridge");
    assert_eq!(bridge_count, 1);

    store
        .update_with_daily_budget(&key_hash, None, None, None, Some(None))
        .await
        .expect("atomic budget clear succeeds")
        .expect("Postgres store supports co-located budget writes")
        .expect("created key still exists");
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM chalk_router.budget_limits \
         WHERE environment_id = $1 AND scope = 'api_key' AND scope_id = $2 \
           AND limit_kind = 'tokens' AND window_size = INTERVAL '1 day'",
    )
    .bind(&environment_id)
    .bind(&key_hash)
    .fetch_one(&pool)
    .await
    .expect("query cleared daily budget");
    assert_eq!(count, 0, "clear removes the daily budget");
    let bridge_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM chalk_router.quota_policies \
         WHERE environment_id = $1 AND budget_limit_id IS NOT NULL",
    )
    .bind(&environment_id)
    .fetch_one(&pool)
    .await
    .expect("count cleared budget bridges");
    assert_eq!(bridge_count, 0);

    assert!(store
        .revoke_with_quota_definitions(&key_hash)
        .await
        .expect("atomic revoke succeeds")
        .expect("Postgres store supports co-located writes"));
    let rate_limit_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM chalk_router.rate_limits \
         WHERE environment_id = $1 AND scope = 'api_key' AND scope_id = $2",
    )
    .bind(&environment_id)
    .bind(&key_hash)
    .fetch_one(&pool)
    .await
    .expect("count rate limits after atomic revoke");
    assert_eq!(rate_limit_count, 0);
    let dependent_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM chalk_router.quota_policies WHERE environment_id = $1",
    )
    .bind(&environment_id)
    .fetch_one(&pool)
    .await
    .expect("count rate-limit dependents after revoke");
    assert_eq!(dependent_count, 0);
}

/// A stored permission that does not parse denies the key instead of loading
/// as the unrestricted default, and updates that leave it alone keep it.
#[tokio::test]
async fn unreadable_stored_permission_denies_the_key() {
    let database_url = std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|value| !value.is_empty());
    let Some(database_url) = database_url else {
        eprintln!("skipping PostgreSQL RPC test; set TEST_DATABASE_URL");
        return;
    };

    openai_server::migrate::run(&database_url)
        .await
        .expect("apply router migrations");
    let connect_options = openai_server::database::connect_options(&database_url)
        .expect("parse disposable PostgreSQL test database URL");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(connect_options)
        .await
        .expect("connect to disposable PostgreSQL test database");

    let environment_id = format!("unreadable-permission-test-{}", uuid::Uuid::new_v4());
    let store = PostgresApiKeyStore::new(
        pool.clone(),
        KeyHasher::new(b"postgres-unreadable-permission-test".to_vec()),
        environment_id.clone(),
    );
    let created = store
        .create(
            "unreadable permission".to_string(),
            ApiKeyPermission {
                connection_id: Some("conn-1".to_string()),
                ..Default::default()
            },
            HashMap::new(),
            None,
        )
        .await
        .expect("create key");
    let key_hash = created.api_key.key_hash.clone();
    for corrupt in [
        serde_json::json!({ "connection_id": ["conn-1"] }),
        serde_json::json!({ "routes": { "refs": [{ "public_model_id": "chalk/a" }] } }),
        serde_json::json!({ "connection_id": "conn-1", "route_ids": ["route-a"], "future": 1 }),
        serde_json::json!({
            "route_ids": ["route-a"],
            "connection_scopes": [{ "connection_id": "conn-1", "modelz": ["gpt-4o"] }]
        }),
    ] {
        sqlx::query(
            "UPDATE chalk_router.api_keys SET permission = $3 \
             WHERE environment_id = $1 AND key_hash = $2",
        )
        .bind(&environment_id)
        .bind(&key_hash)
        .bind(&corrupt)
        .execute(&pool)
        .await
        .expect("corrupt stored permission");

        let key = store
            .authenticate_checked(&created.raw_key)
            .await
            .expect("authentication read succeeds")
            .expect("the key still authenticates");
        assert!(key.permission.unreadable, "{corrupt}");
        assert!(key.permission.is_restricted(), "{corrupt}");
        assert!(!key.permission.grants_model_route(Some("route-a")));
        assert!(key
            .permission
            .allows("openai", Some("conn-1"), "gpt-4o")
            .is_err());

        let renamed = store
            .update(&key_hash, Some("renamed".to_string()), None, None)
            .await
            .expect("update description")
            .expect("key exists");
        assert!(renamed.permission.unreadable);
        let stored: serde_json::Value = sqlx::query_scalar(
            "SELECT permission FROM chalk_router.api_keys \
             WHERE environment_id = $1 AND key_hash = $2",
        )
        .bind(&environment_id)
        .bind(&key_hash)
        .fetch_one(&pool)
        .await
        .expect("read stored permission");
        assert_eq!(stored, corrupt);
    }

    assert!(store
        .update(&key_hash, None, None, Some(ApiKeyPermission::unreadable()))
        .await
        .is_err());
    store.revoke(&key_hash).await.expect("revoke test key");
}
