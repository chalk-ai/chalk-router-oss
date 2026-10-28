//! DB-gated coverage for the invalid-index deployment backstop.

use crate::support::db::{connect_locked, test_database_url};
use sqlx::postgres::PgConnection;

/// Leave behind exactly what a failed `CREATE INDEX CONCURRENTLY` leaves
/// behind: an index Postgres has marked invalid. A unique build over duplicate
/// values is the cheapest way to make one fail.
async fn plant_invalid_index(conn: &mut PgConnection, table_name: &str, index_name: &str) {
    sqlx::query("CREATE SCHEMA IF NOT EXISTS chalk_router")
        .execute(&mut *conn)
        .await
        .expect("create chalk_router schema");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TABLE chalk_router.{table_name} (value INTEGER NOT NULL)"
    )))
    .execute(&mut *conn)
    .await
    .expect("create invalid-index fixture table");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO chalk_router.{table_name} (value) VALUES (1), (1)"
    )))
    .execute(&mut *conn)
    .await
    .expect("seed duplicate values");

    let create_result = sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE UNIQUE INDEX CONCURRENTLY {index_name} \
         ON chalk_router.{table_name} (value)"
    )))
    .execute(&mut *conn)
    .await;
    if create_result.is_ok() {
        // Drop before panicking so the fixture does not outlive the test.
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP TABLE chalk_router.{table_name}"
        )))
        .execute(&mut *conn)
        .await;
        panic!("duplicate values must make the concurrent unique index build fail");
    }
}

fn fixture_names() -> (String, String) {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let suffix = &suffix[..12];
    (
        format!("invalid_index_test_{suffix}"),
        format!("invalid_index_backstop_{suffix}"),
    )
}

#[tokio::test]
async fn migration_fails_when_chalk_router_contains_an_invalid_index() {
    let Some(db_url) =
        test_database_url("migration_fails_when_chalk_router_contains_an_invalid_index")
    else {
        return;
    };

    let (table_name, index_name) = fixture_names();
    let mut conn = connect_locked(&db_url).await;
    plant_invalid_index(&mut conn, &table_name, &index_name).await;

    let migration_result = openai_server::migrate::run(&db_url).await;

    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP TABLE chalk_router.{table_name}"
    )))
    .execute(&mut conn)
    .await
    .expect("clean up invalid-index fixture");

    let error = migration_result.expect_err("invalid index must fail the migration job");
    assert!(
        error.to_string().contains(&index_name),
        "migration error should name the invalid index: {error}"
    );
}

/// Stage the exact dirty state that the newest no-transaction migration can
/// leave: its history is pending and its expected index name exists but is
/// invalid. Recovery must report that index instead of replaying or marking it
/// complete.
#[tokio::test]
async fn invalid_pending_index_is_reported_before_recovery() {
    let Some(db_url) = test_database_url("invalid_pending_index_is_reported_before_recovery")
    else {
        return;
    };

    const INDEX_NAME: &str = "idx_router_api_keys_page";
    const ENVIRONMENT_ID: &str = "migration-invalid-index-recovery";
    let mut conn = connect_locked(&db_url).await;
    openai_server::migrate::run(&db_url)
        .await
        .expect("bring the schema up to date before staging the retry");

    sqlx::query("DROP INDEX CONCURRENTLY chalk_router.idx_router_api_keys_page")
        .execute(&mut conn)
        .await
        .expect("drop the valid index before staging a failed build");
    sqlx::query(
        "INSERT INTO chalk_router.api_keys (environment_id, key_hash, key_prefix) \
         VALUES ($1, 'invalid-index-key-1', 'ck-invalid1'), \
                ($1, 'invalid-index-key-2', 'ck-invalid2') \
         ON CONFLICT DO NOTHING",
    )
    .bind(ENVIRONMENT_ID)
    .execute(&mut conn)
    .await
    .expect("seed duplicate values for the failed unique build");
    let create_result = sqlx::query(
        "CREATE UNIQUE INDEX CONCURRENTLY idx_router_api_keys_page \
         ON chalk_router.api_keys (environment_id)",
    )
    .execute(&mut conn)
    .await;
    assert!(create_result.is_err(), "the unique index build must fail");

    sqlx::query("DELETE FROM chalk_router._sqlx_migrations WHERE version = 6")
        .execute(&mut conn)
        .await
        .expect("remove SQLx history for the pending migration");

    let migration_result = openai_server::migrate::run(&db_url).await;

    // Repair and let the runner recreate both the expected index and history
    // before asserting, so this shared disposable database remains usable by
    // later DB-gated tests.
    sqlx::query("DROP INDEX CONCURRENTLY chalk_router.idx_router_api_keys_page")
        .execute(&mut conn)
        .await
        .expect("drop invalid expected index");
    sqlx::query("DELETE FROM chalk_router.api_keys WHERE environment_id = $1")
        .bind(ENVIRONMENT_ID)
        .execute(&mut conn)
        .await
        .expect("remove duplicate key fixtures");
    openai_server::migrate::run(&db_url)
        .await
        .expect("migration should recover after invalid index is removed");

    let error = migration_result
        .expect_err("invalid index must fail the migration job")
        .to_string();
    assert!(
        error.contains(INDEX_NAME),
        "migration error should name the invalid index: {error}"
    );
    assert!(
        !error.contains("already exists"),
        "the invalid index must be reported before the migration replay: {error}"
    );
}
