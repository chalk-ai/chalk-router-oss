//! DB-gated coverage for canonical rate-limit identities in migration 0021.

use crate::support::db::{connect_locked, test_database_url};

#[tokio::test]
async fn rate_limit_identity_migration_rejects_duplicates_then_normalizes_clean_data() {
    let Some(db_url) = test_database_url(
        "rate_limit_identity_migration_rejects_duplicates_then_normalizes_clean_data",
    ) else {
        return;
    };

    let mut connection = connect_locked(&db_url).await;
    openai_server::migrate::run(&db_url)
        .await
        .expect("bring schema up to date");

    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let environment_id = format!("rate-limit-migration-{suffix}");
    let connection_id = format!("connection-{suffix}");
    let first_id = format!("rate-limit-first-{suffix}");
    let duplicate_id = format!("rate-limit-duplicate-{suffix}");

    sqlx::query(
        "INSERT INTO chalk_router.provider_connections \
         (environment_id, id, name, provider_kind, api_key_envelope) \
         VALUES ($1, $2, 'migration fixture', 'openai', $3)",
    )
    .bind(&environment_id)
    .bind(&connection_id)
    .bind(vec![0_u8])
    .execute(&mut connection)
    .await
    .expect("create connection fixture");
    sqlx::query(
        "DROP INDEX chalk_router.rate_limits_global_identity_unique, \
                    chalk_router.rate_limits_connection_identity_unique",
    )
    .execute(&mut connection)
    .await
    .expect("stage migration indexes");
    sqlx::query(
        "ALTER TABLE chalk_router.rate_limits \
         ADD COLUMN name TEXT NOT NULL DEFAULT ''",
    )
    .execute(&mut connection)
    .await
    .expect("restore pre-migration name column");
    sqlx::query("DELETE FROM chalk_router._sqlx_migrations WHERE version = 21")
        .execute(&mut connection)
        .await
        .expect("stage migration history");
    sqlx::query(
        "INSERT INTO chalk_router.rate_limits \
         (environment_id, id, name, limit_type, limit_value, connection_id, models, \
          burst_value, scope, scope_id, enabled) \
         VALUES \
         ($1, $2, 'first', 'tokens_per_minute', 100, $4, \
          ARRAY['model-b', 'model-a', 'model-a'], 100, 'usage_pool', 'pool-1', false), \
         ($1, $3, 'duplicate', 'tokens_per_minute', 200, $4, \
          ARRAY['model-a', 'model-b'], 200, 'usage_pool', 'pool-1', true)",
    )
    .bind(&environment_id)
    .bind(&first_id)
    .bind(&duplicate_id)
    .bind(&connection_id)
    .execute(&mut connection)
    .await
    .expect("seed duplicate identities");

    let error = openai_server::migrate::run(&db_url)
        .await
        .expect_err("duplicate identities must abort migration")
        .to_string();
    assert!(
        error.contains("cannot add unique rate-limit identity")
            && error.contains(&first_id)
            && error.contains(&duplicate_id),
        "migration should identify the conflicting rows: {error}"
    );

    sqlx::query(
        "DELETE FROM chalk_router.rate_limits \
         WHERE environment_id = $1 AND id = $2",
    )
    .bind(&environment_id)
    .bind(&duplicate_id)
    .execute(&mut connection)
    .await
    .expect("remove duplicate fixture");
    openai_server::migrate::run(&db_url)
        .await
        .expect("clean identity migration should succeed");

    let models: Vec<String> = sqlx::query_scalar(
        "SELECT models FROM chalk_router.rate_limits \
         WHERE environment_id = $1 AND id = $2",
    )
    .bind(&environment_id)
    .bind(&first_id)
    .fetch_one(&mut connection)
    .await
    .expect("read normalized selector");
    assert_eq!(models, ["model-a", "model-b"]);
    let name_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS ( \
            SELECT 1 FROM information_schema.columns \
            WHERE table_schema = 'chalk_router' \
              AND table_name = 'rate_limits' AND column_name = 'name' \
         )",
    )
    .fetch_one(&mut connection)
    .await
    .expect("inspect migrated columns");
    assert!(!name_exists);

    sqlx::query("DELETE FROM chalk_router.rate_limits WHERE environment_id = $1")
        .bind(&environment_id)
        .execute(&mut connection)
        .await
        .expect("clean rate-limit fixture");
    sqlx::query("DELETE FROM chalk_router.provider_connections WHERE environment_id = $1")
        .bind(&environment_id)
        .execute(&mut connection)
        .await
        .expect("clean connection fixture");
}
