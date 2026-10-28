//! Serving-pool recovery uses disposable PostgreSQL, never shared credentials.

use std::{collections::HashMap, time::Duration};

use openai_server::{
    api_keys::{ApiKeyPermission, ApiKeyStore, PostgresApiKeyStore},
    database::{connect_options, serving_pool_options},
    hashing::KeyHasher,
};
use sqlx::{Connection, PgConnection, PgPool};

async fn pool(health_check_idle: Duration) -> Option<(PgPool, PgConnection)> {
    let url = crate::support::db::test_database_url("postgres_pool_health")?;
    let control = crate::support::db::connect_locked(&url).await;
    openai_server::migrate::run(&url).await.unwrap();
    let pool = serving_pool_options(1, Duration::from_secs(3), health_check_idle)
        .connect_with(connect_options(&url).unwrap())
        .await
        .unwrap();
    Some((pool, control))
}

async fn wait_idle(pool: &PgPool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while pool.num_idle() != 1 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("release validation should return the connection to the pool");
}

async fn kill_returned_connection(pool: &PgPool, control: &mut PgConnection) -> i32 {
    let mut connection = pool.acquire().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    drop(connection);
    wait_idle(pool).await;
    let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .fetch_one(&mut *control)
        .await
        .unwrap();
    assert!(terminated);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1)")
                    .bind(pid)
                    .fetch_one(&mut *control)
                    .await
                    .unwrap();
            if !exists {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("backend should exit after termination");
    pid
}

#[tokio::test]
async fn postgres_idle_acquisition_replaces_killed_connections() {
    let Some((pool, mut control)) = pool(Duration::from_millis(10)).await else {
        return;
    };
    let old_pid = kill_returned_connection(&pool, &mut control).await;
    tokio::time::sleep(Duration::from_millis(15)).await;
    let new_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&pool)
        .await
        .expect("idle health check should replace the terminated connection");
    assert_ne!(old_pid, new_pid);
    pool.close().await;
}

#[tokio::test]
async fn postgres_recent_connection_authentication_recovers_without_false_401() {
    let Some((pool, mut control)) = pool(Duration::from_secs(60)).await else {
        return;
    };
    let store = PostgresApiKeyStore::new(
        pool.clone(),
        KeyHasher::new(b"postgres-pool-health-test".to_vec()),
        format!("pool-health-{}", uuid::Uuid::new_v4()),
    );
    let created = store
        .create(
            "pool health test".to_string(),
            ApiKeyPermission::default(),
            HashMap::new(),
            None,
        )
        .await
        .unwrap();

    // First prove fresh acquisition skips its ping: the real query observes
    // termination. SQLx's release validation must then discard the socket.
    kill_returned_connection(&pool, &mut control).await;
    assert!(sqlx::query("SELECT 1").execute(&pool).await.is_err());
    sqlx::query("SELECT 1").execute(&pool).await.unwrap();

    // The authentication read alone retries this safe, read-only operation.
    kill_returned_connection(&pool, &mut control).await;
    let authenticated = store
        .authenticate_checked(&created.raw_key)
        .await
        .expect("transport failure should retry the lookup once")
        .expect("existing API key should still authenticate");
    assert_eq!(authenticated.key_hash, created.api_key.key_hash);
    assert!(store
        .authenticate_checked("invalid-key")
        .await
        .unwrap()
        .is_none());
    store.revoke(&created.api_key.key_hash).await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_release_validation_drains_canceled_query_and_rolls_back() {
    let Some((pool, _control)) = pool(Duration::from_secs(60)).await else {
        return;
    };
    let mut connection = pool.acquire().await.unwrap();
    let mut transaction = connection.begin().await.unwrap();
    sqlx::query("SELECT txid_current()")
        .execute(&mut *transaction)
        .await
        .expect("assign an xid so a leaked transaction is observable");
    assert!(tokio::time::timeout(
        Duration::from_millis(10),
        sqlx::query("SELECT pg_sleep(0.1)").execute(&mut *transaction),
    )
    .await
    .is_err());
    drop(transaction);
    drop(connection);
    wait_idle(&pool).await;
    let current_transaction: Option<i64> =
        sqlx::query_scalar("SELECT txid_current_if_assigned()::bigint")
            .fetch_one(&pool)
            .await
            .expect("returned connection should be synchronized after cancellation");
    assert!(
        current_transaction.is_none(),
        "canceled transaction must be rolled back"
    );
    pool.close().await;
}

#[tokio::test]
async fn postgres_zero_idle_interval_restores_unconditional_health_checks() {
    let Some((pool, mut control)) = pool(Duration::ZERO).await else {
        return;
    };
    let old_pid = kill_returned_connection(&pool, &mut control).await;
    let new_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&pool)
        .await
        .expect("unconditional health checks should replace even freshly killed connections");
    assert_ne!(old_pid, new_pid);
    pool.close().await;
}
