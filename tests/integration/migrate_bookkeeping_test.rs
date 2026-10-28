//! DB-gated coverage for the router's SQLx 0.9 migration runner, split one
//! test per concern: qualified history isolated from an unrelated public
//! co-tenant table, non-transactional crash recovery, concurrent runners, and
//! importing history that leaked entirely into `public` by a legacy runner,
//! and preserving routes when adding route-owned routing configuration.
//! `migrate::run` is idempotent against an already-migrated database, so each
//! test below establishes its own baseline rather than depending on nextest's
//! run order; all tests serialize on one process-wide advisory lock
//! against the shared throwaway database.

use crate::support::db::{connect_locked, test_database_url};

/// Includes case-sensitive usage-pool name uniqueness (0034); count the `.sql` files under `migrations/default/` rather than hardcoding
/// the current migration count, so a new migration doesn't have to touch this
/// test to keep it passing (`AGENTS.md` still requires updating it in the same
/// PR if the runner's bookkeeping behavior itself changes).
fn migration_file_count() -> i64 {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations/default");
    std::fs::read_dir(&dir)
        .expect("read migrations/default")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "sql"))
        .count() as i64
}

#[tokio::test]
async fn model_route_routing_migration_preserves_existing_routes_and_legacy_writes() {
    let Some(db_url) = test_database_url(
        "model_route_routing_migration_preserves_existing_routes_and_legacy_writes",
    ) else {
        return;
    };
    let mut conn = connect_locked(&db_url).await;
    openai_server::migrate::run(&db_url)
        .await
        .expect("establish migrated baseline");

    // Restore the old table shape so this exercises an upgrade with existing
    // data, rather than only creating the new column on an empty database.
    sqlx::query("ALTER TABLE chalk_router.model_routes DROP COLUMN routing")
        .execute(&mut conn)
        .await
        .expect("restore pre-routing table shape");
    sqlx::query("DELETE FROM chalk_router._sqlx_migrations WHERE version = 26")
        .execute(&mut conn)
        .await
        .expect("stage routing migration");

    let environment_id = format!("route-migration-{}", uuid::Uuid::new_v4());
    sqlx::query(
        "INSERT INTO chalk_router.model_routes \
         (environment_id, id, public_model_id, connection_id, upstream_model_id, published) \
         VALUES ($1, 'existing', 'chalk/existing', 'connection-a', 'astra', false)",
    )
    .bind(&environment_id)
    .execute(&mut conn)
    .await
    .expect("create pre-migration route");
    let timestamps: (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as(
            "SELECT created_at, updated_at FROM chalk_router.model_routes \
             WHERE environment_id = $1 AND id = 'existing'",
        )
        .bind(&environment_id)
        .fetch_one(&mut conn)
        .await
        .unwrap();

    openai_server::migrate::run(&db_url)
        .await
        .expect("upgrade existing routes");
    let route: (String, String, String, bool, serde_json::Value) = sqlx::query_as(
        "SELECT public_model_id, connection_id, upstream_model_id, published, routing \
         FROM chalk_router.model_routes WHERE environment_id = $1 AND id = 'existing'",
    )
    .bind(&environment_id)
    .fetch_one(&mut conn)
    .await
    .unwrap();
    assert_eq!(
        route,
        (
            "chalk/existing".into(),
            "connection-a".into(),
            "astra".into(),
            false,
            serde_json::json!({}),
        )
    );
    let after: (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) = sqlx::query_as(
        "SELECT created_at, updated_at FROM chalk_router.model_routes \
         WHERE environment_id = $1 AND id = 'existing'",
    )
    .bind(&environment_id)
    .fetch_one(&mut conn)
    .await
    .unwrap();
    assert_eq!(after, timestamps);

    let legacy_default: serde_json::Value = sqlx::query_scalar(
        "INSERT INTO chalk_router.model_routes \
         (environment_id, id, public_model_id, connection_id, upstream_model_id) \
         VALUES ($1, 'legacy', 'chalk/legacy', 'connection-a', 'astra') RETURNING routing",
    )
    .bind(&environment_id)
    .fetch_one(&mut conn)
    .await
    .expect("old writers may omit routing after the upgrade");
    assert_eq!(legacy_default, serde_json::json!({}));

    let grid = serde_json::json!({
        "default_target": {"connection_id": "connection-a", "upstream_model_id": "astra"},
        "fallbacks": {"connection-a/astra": ["connection-b/gpt-5.5"]},
        "judge": {
            "mode": "live",
            "classification": "task",
            "backend": {
                "type": "completion",
                "target": {"connection_id": "connection-a", "upstream_model_id": "classifier"},
                "timeout_ms": 1000
            },
            "outcomes": {
                "easy": {"model": "connection-a/gpt-4o", "description": "Simple tasks", "fallbacks": []}
            }
        }
    });
    let persisted: serde_json::Value = sqlx::query_scalar(
        "UPDATE chalk_router.model_routes SET routing = $2 \
         WHERE environment_id = $1 AND id = 'existing' RETURNING routing",
    )
    .bind(&environment_id)
    .bind(&grid)
    .fetch_one(&mut conn)
    .await
    .expect("store a route grid as JSON");
    assert_eq!(persisted, grid);

    for invalid in [serde_json::Value::Null, serde_json::json!([])] {
        let error = sqlx::query(
            "UPDATE chalk_router.model_routes SET routing = $2 \
             WHERE environment_id = $1 AND id = 'existing'",
        )
        .bind(&environment_id)
        .bind(invalid)
        .execute(&mut conn)
        .await
        .expect_err("routing must be a JSON object");
        assert_eq!(
            error.as_database_error().unwrap().constraint(),
            Some("model_routes_routing_object")
        );
    }

    sqlx::query("DELETE FROM chalk_router.model_routes WHERE environment_id = $1")
        .bind(&environment_id)
        .execute(&mut conn)
        .await
        .expect("remove route fixtures");
}

#[tokio::test]
async fn router_migrations_use_a_qualified_history_table_isolated_from_a_public_cotenant() {
    let Some(db_url) = test_database_url(
        "router_migrations_use_a_qualified_history_table_isolated_from_a_public_cotenant",
    ) else {
        return;
    };
    let mut conn = connect_locked(&db_url).await;

    // A co-tenant may own an unrelated public table with SQLx's conventional
    // name. The router must neither require its shape nor modify it.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS public._sqlx_migrations \
         (version BIGINT PRIMARY KEY, sentinel TEXT)",
    )
    .execute(&mut conn)
    .await
    .expect("create public sentinel");
    sqlx::query(
        "INSERT INTO public._sqlx_migrations (version, sentinel) VALUES (-1, 'keep') \
         ON CONFLICT (version) DO UPDATE SET sentinel = 'keep'",
    )
    .execute(&mut conn)
    .await
    .expect("seed public sentinel");

    openai_server::migrate::run(&db_url)
        .await
        .expect("fresh migration should succeed");

    let history_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM chalk_router._sqlx_migrations")
            .fetch_one(&mut conn)
            .await
            .expect("count qualified history");
    assert_eq!(history_count, migration_file_count());
    let accounting_table: bool = sqlx::query_scalar(
        "SELECT to_regclass('chalk_router.background_response_jobs') IS NOT NULL",
    )
    .fetch_one(&mut conn)
    .await
    .expect("background accounting migration");
    assert!(accounting_table);
    let ownership_columns: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.columns WHERE table_schema='chalk_router' AND table_name IN ('quota_reservations','quota_leases') AND column_name IN ('background_environment_id','background_event_id')",
    )
    .fetch_one(&mut conn)
    .await
    .expect("background quota ownership migration");
    assert_eq!(ownership_columns, 4);
    let grants_table: bool = sqlx::query_scalar(
        "SELECT to_regclass('chalk_router.background_response_quota_grants') IS NOT NULL",
    )
    .fetch_one(&mut conn)
    .await
    .expect("no separate background grants table");
    assert!(!grants_table);
    let sentinel: Option<String> =
        sqlx::query_scalar("SELECT sentinel FROM public._sqlx_migrations WHERE version = -1")
            .fetch_optional(&mut conn)
            .await
            .expect("read public sentinel");
    assert_eq!(sentinel.as_deref(), Some("keep"));

    sqlx::query("DROP TABLE public._sqlx_migrations")
        .execute(&mut conn)
        .await
        .expect("clean up public fixture");
}

#[tokio::test]
async fn router_migrations_recover_an_unrecorded_but_already_applied_concurrent_index() {
    let Some(db_url) = test_database_url(
        "router_migrations_recover_an_unrecorded_but_already_applied_concurrent_index",
    ) else {
        return;
    };
    let mut conn = connect_locked(&db_url).await;
    openai_server::migrate::run(&db_url)
        .await
        .expect("baseline migration should succeed");

    // Simulate a crash after the final CREATE INDEX CONCURRENTLY committed but
    // before SQLx recorded its history. The verified index should be adopted.
    sqlx::query("DELETE FROM chalk_router._sqlx_migrations WHERE version = 6")
        .execute(&mut conn)
        .await
        .expect("stage unrecorded concurrent index");
    openai_server::migrate::run(&db_url)
        .await
        .expect("valid unrecorded index should be recovered");
    let recovered: bool = sqlx::query_scalar(
        "SELECT EXISTS (\
            SELECT 1 FROM chalk_router._sqlx_migrations \
            WHERE version = 6 AND success\
        )",
    )
    .fetch_one(&mut conn)
    .await
    .expect("check recovered history");
    assert!(recovered);
}

#[tokio::test]
async fn concurrent_migration_runners_serialize_without_sqlxs_session_advisory_lock() {
    let Some(db_url) = test_database_url(
        "concurrent_migration_runners_serialize_without_sqlxs_session_advisory_lock",
    ) else {
        return;
    };
    let _conn = connect_locked(&db_url).await;
    openai_server::migrate::run(&db_url)
        .await
        .expect("baseline migration should succeed");

    // New runners serialize on the router's own lock, not SQLx's session
    // advisory lock, which only protects one connection's own migration run.
    let (first, second) = tokio::join!(
        openai_server::migrate::run(&db_url),
        openai_server::migrate::run(&db_url),
    );
    first.expect("first concurrent runner should succeed");
    second.expect("second concurrent runner should succeed");
}

#[tokio::test]
async fn router_migrations_import_history_leaked_entirely_into_public_by_a_legacy_runner() {
    let Some(db_url) = test_database_url(
        "router_migrations_import_history_leaked_entirely_into_public_by_a_legacy_runner",
    ) else {
        return;
    };
    let mut conn = connect_locked(&db_url).await;
    openai_server::migrate::run(&db_url)
        .await
        .expect("baseline migration should succeed");

    // Reproduce the legacy PgBouncer failure: router objects exist, but the
    // old unqualified runner's exact history landed in public. Unrelated public
    // rows remain ignored even when exact router rows are also present.
    sqlx::query("DROP TABLE IF EXISTS public._sqlx_migrations")
        .execute(&mut conn)
        .await
        .expect("clear any leftover public fixture");
    sqlx::query(
        "CREATE TABLE public._sqlx_migrations \
         (LIKE chalk_router._sqlx_migrations INCLUDING ALL)",
    )
    .execute(&mut conn)
    .await
    .expect("create leaked legacy history");
    sqlx::query(
        "INSERT INTO public._sqlx_migrations \
         SELECT * FROM chalk_router._sqlx_migrations",
    )
    .execute(&mut conn)
    .await
    .expect("copy exact legacy history");
    sqlx::query(
        "INSERT INTO public._sqlx_migrations \
         (version, description, success, checksum, execution_time) \
         VALUES (999, 'foreign migration', TRUE, '\\xdeadbeef', 1)",
    )
    .execute(&mut conn)
    .await
    .expect("add unrelated public history");
    sqlx::query("DELETE FROM chalk_router._sqlx_migrations")
        .execute(&mut conn)
        .await
        .expect("stage history leaked entirely to public");

    openai_server::migrate::run(&db_url)
        .await
        .expect("exact public router history should be imported");
    let imported: i64 = sqlx::query_scalar("SELECT count(*) FROM chalk_router._sqlx_migrations")
        .fetch_one(&mut conn)
        .await
        .expect("count imported history");
    assert_eq!(imported, migration_file_count());
    let foreign_retained: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM public._sqlx_migrations WHERE version = 999)",
    )
    .fetch_one(&mut conn)
    .await
    .expect("check foreign history");
    assert!(foreign_retained);

    sqlx::query("DROP TABLE public._sqlx_migrations")
        .execute(&mut conn)
        .await
        .expect("clean up public fixture");
}
