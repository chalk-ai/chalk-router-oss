//! Exact-name uniqueness upgrades preserve existing names and fail on duplicates.

use sqlx::PgConnection;

use crate::support::db::{connect_locked, test_database_url};

async fn names(
    connection: &mut PgConnection,
    environment: &str,
) -> Vec<(String, String, chrono::DateTime<chrono::Utc>)> {
    sqlx::query_as("SELECT id,name,created_at FROM chalk_router.usage_pools WHERE environment_id=$1 ORDER BY id")
        .bind(environment).fetch_all(connection).await.unwrap()
}

#[tokio::test]
async fn migration_preserves_exact_names_and_rolls_back_on_duplicates() {
    let Some(url) = test_database_url("usage_pool_names_migration") else {
        return;
    };
    let mut connection = connect_locked(&url).await;
    openai_server::migrate::run(&url).await.unwrap();
    sqlx::raw_sql(
        "DROP INDEX chalk_router.uq_router_usage_pools_name;
        DELETE FROM chalk_router._sqlx_migrations WHERE version=34;",
    )
    .execute(&mut connection)
    .await
    .unwrap();

    let environment = format!("pool-name-upgrade-{}", uuid::Uuid::new_v4());
    let isolated_environment = format!("{environment}-isolated");
    for (id, name) in [
        ("a", "Production"),
        ("b", "production"),
        ("c", " Production "),
        ("duplicate", "Production"),
    ] {
        sqlx::query("INSERT INTO chalk_router.usage_pools (environment_id,id,name,created_at) VALUES ($1,$2,$3,'2025-01-01T00:00:00Z')")
            .bind(&environment).bind(id).bind(name).execute(&mut connection).await.unwrap();
    }
    sqlx::query("INSERT INTO chalk_router.usage_pools (environment_id,id,name) VALUES ($1,'a','Production')")
        .bind(&isolated_environment).execute(&mut connection).await.unwrap();
    let before = names(&mut connection, &environment).await;

    // The migration assumes exact names are already unique. An unexpected
    // duplicate fails without modifying data or recording a successful upgrade.
    let error = openai_server::migrate::run(&url).await.unwrap_err();
    assert!(
        error.to_string().contains("uq_router_usage_pools_name"),
        "unexpected migration failure: {error}"
    );
    assert_eq!(names(&mut connection, &environment).await, before);
    let applied: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM chalk_router._sqlx_migrations WHERE version=34)",
    )
    .fetch_one(&mut connection)
    .await
    .unwrap();
    assert!(!applied);
    let index: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('chalk_router.uq_router_usage_pools_name')::text")
            .fetch_one(&mut connection)
            .await
            .unwrap();
    assert!(index.is_none());

    // Remove the conflicting fixture and retry the actual migration runner.
    sqlx::query("DELETE FROM chalk_router.usage_pools WHERE environment_id=$1 AND id='duplicate'")
        .bind(&environment)
        .execute(&mut connection)
        .await
        .unwrap();
    let before = names(&mut connection, &environment).await;
    let isolated_before = names(&mut connection, &isolated_environment).await;
    let report = openai_server::migrate::run(&url).await.unwrap();
    assert_eq!(report.applied, 1);
    assert_eq!(names(&mut connection, &environment).await, before);
    assert_eq!(
        names(&mut connection, &isolated_environment).await,
        isolated_before
    );
    let error = sqlx::query("INSERT INTO chalk_router.usage_pools (environment_id,id,name) VALUES ($1,'new','Production')")
        .bind(&environment).execute(&mut connection).await.unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().constraint(),
        Some("uq_router_usage_pools_name")
    );
    assert_eq!(openai_server::migrate::run(&url).await.unwrap().applied, 0);

    for environment in [&environment, &isolated_environment] {
        sqlx::query("DELETE FROM chalk_router.usage_pools WHERE environment_id=$1")
            .bind(environment)
            .execute(&mut connection)
            .await
            .unwrap();
    }
}
