//! Separate-connection races proving state and receipt lock ordering.
use chrono::Utc;

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

async fn seed(pool: &sqlx::PgPool) -> (String, String, serde_json::Value) {
    let env = format!("quota-lock-{}", uuid::Uuid::new_v4());
    let c = format!("{env}:0-concurrency");
    let t = format!("{env}:1-token");
    for id in [&c, &t] {
        sqlx::query("WITH b AS (INSERT INTO chalk_router.budget_limits(environment_id,id,scope,scope_id,window_type,window_size,limit_kind,limit_amount,enabled) VALUES($1,$2,'api_key',$2,'calendar',INTERVAL '1 day','tokens',1000,true)) INSERT INTO chalk_router.quota_policies(id,environment_id,budget_limit_id) VALUES($2,$1,$2)")
            .bind(&env).bind(id).execute(pool).await.unwrap();
    }
    // Deliberately reverse caller order. The database must impose its order,
    // including first-use INSERT/unique conflicts, independently of input.
    let start = Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc();
    let actions = serde_json::json!([
        {"policy_id": t, "environment_id": env, "source":"budget_limit", "algorithm":"fixed_window", "meter":"tokens", "limit_value":1000, "amount":10, "window_start": start, "window_end":start + chrono::Duration::days(1)},
        {"policy_id": c, "environment_id": env, "source":"rate_limit", "algorithm":"concurrency", "meter":"concurrent_requests", "limit_value":1000, "amount":1}
    ]);
    (c, t, actions)
}

async fn wait_blocked(pool: &sqlx::PgPool, blocked: i32, blocker: i32) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let yes: bool = sqlx::query_scalar("SELECT $2 = ANY(pg_blocking_pids($1))")
                .bind(blocked)
                .bind(blocker)
                .fetch_one(pool)
                .await
                .unwrap();
            if yes {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker must be waiting on the held policy, before continuing race");
}

#[tokio::test]
async fn postgres_first_use_unique_conflicts_follow_policy_order() {
    let Some(pool) = pool().await else { return };
    let (c, t, actions) = seed(&pool).await;
    let mut holder = pool.begin().await.unwrap();
    let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    sqlx::query("INSERT INTO chalk_router.quota_states(policy_id,active_count) VALUES($1,0)")
        .bind(&c)
        .execute(&mut *holder)
        .await
        .unwrap();
    let mut worker = pool.acquire().await.unwrap();
    let worker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *worker)
        .await
        .unwrap();
    let permit = uuid::Uuid::new_v4();
    let worker_actions = actions.clone();
    let task = tokio::spawn(async move {
        sqlx::query_scalar::<_, bool>(
            "SELECT allowed FROM chalk_router.acquire_quota($1,$2,60,300)",
        )
        .bind(worker_actions)
        .bind(permit)
        .fetch_one(&mut *worker)
        .await
    });
    wait_blocked(&pool, worker_pid, holder_pid).await;
    // Unordered insertion would have inserted T before blocking on C, so this
    // creates the opposing wait deterministically on the old implementation.
    tokio::time::timeout(std::time::Duration::from_secs(3), sqlx::query("INSERT INTO chalk_router.quota_states(policy_id,used_amount,window_start) VALUES($1,0,$2) ON CONFLICT DO NOTHING").bind(&t).bind(Utc::now().date_naive().and_hms_opt(0,0,0).unwrap().and_utc()).execute(&mut *holder)).await.unwrap().unwrap();
    holder.commit().await.unwrap();
    assert!(task.await.unwrap().unwrap());
    sqlx::query("SELECT chalk_router.finish_quota($1,$2,4)")
        .bind(&actions)
        .bind(permit)
        .execute(&pool)
        .await
        .unwrap();
    let used: i64 =
        sqlx::query_scalar("SELECT used_amount FROM chalk_router.quota_states WHERE policy_id=$1")
            .bind(&t)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(used, 4);
}

#[tokio::test]
async fn postgres_cleanup_prelocks_states_before_receipts_and_finish_is_idempotent() {
    let Some(pool) = pool().await else { return };
    let (c, t, actions) = seed(&pool).await;
    let permit = uuid::Uuid::new_v4();
    let allowed: bool =
        sqlx::query_scalar("SELECT allowed FROM chalk_router.acquire_quota($1,$2,60,300)")
            .bind(&actions)
            .bind(permit)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(allowed);
    for statement in ["UPDATE chalk_router.quota_reservations SET expires_at=now()-INTERVAL '1 second' WHERE permit_id=$1", "UPDATE chalk_router.quota_leases SET expires_at=now()-INTERVAL '1 second' WHERE permit_id=$1"] {
        sqlx::query(statement).bind(permit).execute(&pool).await.unwrap();
    }
    let mut holder = pool.begin().await.unwrap();
    let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    sqlx::query("SELECT 1 FROM chalk_router.quota_states WHERE policy_id=$1 FOR UPDATE")
        .bind(&c)
        .execute(&mut *holder)
        .await
        .unwrap();
    let mut worker = pool.acquire().await.unwrap();
    let worker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *worker)
        .await
        .unwrap();
    let task = tokio::spawn(async move {
        sqlx::query_as::<_, (i64, i64)>("SELECT * FROM chalk_router.cleanup_quota_runtime(100000)")
            .fetch_one(&mut *worker)
            .await
    });
    wait_blocked(&pool, worker_pid, holder_pid).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        sqlx::query("SELECT chalk_router.finish_quota($1,$2,4)")
            .bind(&actions)
            .bind(permit)
            .execute(&mut *holder),
    )
    .await
    .unwrap()
    .unwrap();
    holder.commit().await.unwrap();
    task.await.unwrap().unwrap();
    sqlx::query("SELECT chalk_router.finish_quota($1,$2,999)")
        .bind(&actions)
        .bind(permit)
        .execute(&pool)
        .await
        .unwrap();
    let state: (i64,i64) = sqlx::query_as("SELECT (SELECT used_amount FROM chalk_router.quota_states WHERE policy_id=$1),(SELECT active_count FROM chalk_router.quota_states WHERE policy_id=$2)").bind(&t).bind(&c).fetch_one(&pool).await.unwrap();
    assert_eq!(state, (4, 0));
    let receipts: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM chalk_router.quota_reservations WHERE permit_id=$1)+(SELECT count(*) FROM chalk_router.quota_leases WHERE permit_id=$1)").bind(permit).fetch_one(&pool).await.unwrap();
    assert_eq!(receipts, 0);
}

#[tokio::test]
async fn postgres_acquire_and_finish_share_order_and_unknown_usage_stays_reserved() {
    let Some(pool) = pool().await else { return };
    let (c, t, actions) = seed(&pool).await;
    let first = uuid::Uuid::new_v4();
    let second = uuid::Uuid::new_v4();
    let allowed: bool =
        sqlx::query_scalar("SELECT allowed FROM chalk_router.acquire_quota($1,$2,60,300)")
            .bind(&actions)
            .bind(first)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(allowed);
    let mut holder = pool.begin().await.unwrap();
    let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    sqlx::query("SELECT 1 FROM chalk_router.quota_states WHERE policy_id=$1 FOR UPDATE")
        .bind(&c)
        .execute(&mut *holder)
        .await
        .unwrap();
    let mut worker = pool.acquire().await.unwrap();
    let worker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *worker)
        .await
        .unwrap();
    let worker_actions = actions.clone();
    let task = tokio::spawn(async move {
        sqlx::query("SELECT chalk_router.finish_quota($1,$2,4)")
            .bind(worker_actions)
            .bind(first)
            .execute(&mut *worker)
            .await
    });
    wait_blocked(&pool, worker_pid, holder_pid).await;
    // If settlement locks token state T before concurrency C, acquisition here
    // waits on T while that settlement waits on C. Sorted prelocking prevents it.
    let allowed: bool = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        sqlx::query_scalar("SELECT allowed FROM chalk_router.acquire_quota($1,$2,60,300)")
            .bind(&actions)
            .bind(second)
            .fetch_one(&mut *holder),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(allowed);
    holder.commit().await.unwrap();
    task.await.unwrap().unwrap();
    sqlx::query("SELECT chalk_router.finish_quota($1,$2,NULL)")
        .bind(&actions)
        .bind(second)
        .execute(&pool)
        .await
        .unwrap();
    let state: (i64,i64) = sqlx::query_as("SELECT (SELECT used_amount FROM chalk_router.quota_states WHERE policy_id=$1),(SELECT active_count FROM chalk_router.quota_states WHERE policy_id=$2)").bind(&t).bind(&c).fetch_one(&pool).await.unwrap();
    assert_eq!(
        state,
        (14, 0),
        "known usage reconciles; unknown usage keeps the conservative reservation"
    );
}

#[tokio::test]
async fn postgres_two_phase_admission_prelocks_union_before_request_phase() {
    let Some(pool) = pool().await else { return };
    let (c, t, actions) = seed(&pool).await;
    let mut holder = pool.begin().await.unwrap();
    let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    sqlx::query("INSERT INTO chalk_router.quota_states(policy_id,active_count) VALUES($1,0)")
        .bind(&c)
        .execute(&mut *holder)
        .await
        .unwrap();
    let mut worker = pool.acquire().await.unwrap();
    let worker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *worker)
        .await
        .unwrap();
    // The request phase owns T, and the route phase owns C. Taking only each
    // phase's locks would insert/lock T before waiting for the lower policy C.
    let request_actions = serde_json::json!([actions[0]]);
    let route_actions = serde_json::json!([actions[1]]);
    let task = tokio::spawn(async move {
        sqlx::query_scalar::<_, bool>("SELECT (request_decision->>'allowed')::BOOLEAN AND (route_decision->>'allowed')::BOOLEAN FROM chalk_router.acquire_request_and_route_quota($1,$2,$3,$4,60,300,NULL)")
            .bind(request_actions).bind(route_actions)
            .bind(uuid::Uuid::new_v4()).bind(uuid::Uuid::new_v4())
            .fetch_one(&mut *worker).await
    });
    wait_blocked(&pool, worker_pid, holder_pid).await;
    tokio::time::timeout(std::time::Duration::from_secs(3),
        sqlx::query("INSERT INTO chalk_router.quota_states(policy_id,used_amount,window_start) VALUES($1,0,$2) ON CONFLICT DO NOTHING")
            .bind(&t).bind(Utc::now().date_naive().and_hms_opt(0,0,0).unwrap().and_utc())
            .execute(&mut *holder)
    ).await.expect("union locking must leave higher policy free while waiting for lower policy").unwrap();
    holder.commit().await.unwrap();
    assert!(task.await.unwrap().unwrap());
}
