//! Advisory-lock guard for the Postgres migration integration tests
//! (`migrate_bookkeeping_test`, `migrate_invalid_index_test`,
//! `rate_limit_migration_test`). All three run `openai_server::migrate::run`
//! and mutate the one `chalk_router` schema in whatever database
//! `TEST_DATABASE_URL` names, so they must not run concurrently with each
//! other: a stuck `CREATE INDEX CONCURRENTLY` blocks the nextest runner, and
//! Postgres does not detect that as a deadlock. Take the advisory lock before
//! running migrations or touching schema.
//!
//! The `postgres` nextest test group (`.config/nextest.toml`) pins these
//! three, plus every other test that shares the same throwaway database, to
//! one thread as a second layer of protection: `postgres_atomic_key_writes_test`,
//! the `postgres_*`-named tests in `ratelimit_conformance_test`,
//! `usage_cost_test`, and `usage_snapshot_test`. Route-grid Postgres conformance
//! tests and moderation usage tests also join this group and take the advisory lock. The other additional
//! tests don't take this advisory lock — they serialize on per-row uniqueness within the
//! shared schema instead — so update this list (and `.config/nextest.toml`'s
//! filter) together whenever a Postgres-gated test is added, renamed, or
//! removed. This lock also protects a plain `cargo test`/`cargo nextest` run
//! with a higher thread count. `quota_hot_path_test::postgres_*` and
//! `quota_lock_order_test::postgres_*` also join the serialized nextest group
//! for deterministic multi-connection races.
//! `postgres_pool_health_test` and `gemini_embedding_quota_test::postgres` use
//! the same lock and serialized test group.
//! Usage-pool name migration and Postgres conformance tests also take this lock
//! and join that group.

use sqlx::postgres::PgConnection;
use sqlx::Connection;

const MIGRATION_TEST_ADVISORY_LOCK: i64 = 4_207_332_002;

/// `TEST_DATABASE_URL`, or `None` when this optional suite is not opted into.
pub fn test_database_url(test_name: &str) -> Option<String> {
    let db_url = std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|value| !value.is_empty());
    if db_url.is_none() {
        eprintln!("skipping {test_name}: set TEST_DATABASE_URL");
    }
    db_url
}

/// Connect and serialize against the other migration integration tests, which
/// all mutate the one shared `chalk_router` schema.
pub async fn connect_locked(db_url: &str) -> PgConnection {
    let mut conn = PgConnection::connect(db_url).await.expect("connect");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(MIGRATION_TEST_ADVISORY_LOCK)
        .execute(&mut conn)
        .await
        .expect("serialize migration integration tests");
    conn
}
