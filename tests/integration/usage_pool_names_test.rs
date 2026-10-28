//! The same name identity and conflict behavior must hold in both stores.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use openai_server::{
    budget::{self, ApiKeyBudgetInput, BudgetPolicyStore},
    ratelimit::{
        self, ApiKeyRateLimitInput, RateLimitMatch, RateLimitPolicy, RateLimitPolicyStore,
    },
    service::{
        usage_pools::{
            CreateUsagePoolInput, UpdateUsagePoolInput, UsagePoolLookup, UsagePoolService,
        },
        ServiceError,
    },
    usage_pools::{
        InMemoryUsagePoolStore, PostgresUsagePoolStore, UsagePoolStore, UsagePoolStoreError,
    },
};
use tokio::sync::Barrier;

async fn store_suite(store: Arc<dyn UsagePoolStore>) {
    let first = store.create_pool("Production".into()).await.unwrap();
    assert_eq!(
        store
            .get_pool_by_name("Production")
            .await
            .unwrap()
            .unwrap()
            .id,
        first.id
    );
    for name in ["production", " Production ", "missing"] {
        assert!(store.get_pool_by_name(name).await.unwrap().is_none());
    }
    assert!(matches!(
        store.create_pool("Production".into()).await,
        Err(UsagePoolStoreError::DuplicateName)
    ));
    let other = store.create_pool("Other".into()).await.unwrap();
    assert!(matches!(
        store.update_pool(&other.id, "Production".into()).await,
        Err(UsagePoolStoreError::DuplicateName)
    ));
    assert_eq!(
        store.get_pool(&other.id).await.unwrap().unwrap().name,
        "Other"
    );
    assert!(store
        .update_pool("missing", "Production".into())
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        store
            .update_pool(&first.id, "PRODUCTION".into())
            .await
            .unwrap()
            .unwrap()
            .name,
        "PRODUCTION"
    );
    assert_eq!(
        store
            .update_pool(&first.id, "PRODUCTION".into())
            .await
            .unwrap()
            .unwrap()
            .id,
        first.id
    );
    assert!(store
        .get_pool_by_name("Production")
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        store
            .get_pool_by_name("PRODUCTION")
            .await
            .unwrap()
            .unwrap()
            .id,
        first.id
    );
    assert!(store.delete_pool(&first.id).await.unwrap());
    assert!(store
        .get_pool_by_name("PRODUCTION")
        .await
        .unwrap()
        .is_none());
    store.create_pool("production".into()).await.unwrap();

    // Stores compare the exact name. Trimming belongs to the service.
    for name in [
        "Production",
        " production ",
        "\tPRODUCTION\u{3000}",
        "Ä",
        "ä",
        "Production (1)",
        "Pro duction",
        "Pro\u{200b}duction",
    ] {
        let created = store.create_pool(name.into()).await.unwrap();
        assert_eq!(
            store.get_pool_by_name(name).await.unwrap().unwrap().id,
            created.id
        );
    }

    let barrier = Arc::new(Barrier::new(8));
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let store = store.clone();
        let barrier = barrier.clone();
        tasks.spawn(async move {
            barrier.wait().await;
            store.create_pool("Concurrent".into()).await
        });
    }
    let mut successes = 0;
    while let Some(result) = tasks.join_next().await {
        match result.unwrap() {
            Ok(_) => successes += 1,
            Err(UsagePoolStoreError::DuplicateName) => {}
            Err(error) => panic!("unexpected create error: {error}"),
        }
    }
    assert_eq!(successes, 1);

    let left = store.create_pool("Left".into()).await.unwrap();
    let right = store.create_pool("Right".into()).await.unwrap();
    let barrier = Arc::new(Barrier::new(2));
    for id in [left.id, right.id] {
        let store = store.clone();
        let barrier = barrier.clone();
        tasks.spawn(async move {
            barrier.wait().await;
            store
                .update_pool(&id, "Contended rename".into())
                .await
                .map(|pool| pool.unwrap())
        });
    }
    let mut successes = 0;
    while let Some(result) = tasks.join_next().await {
        match result.unwrap() {
            Ok(_) => successes += 1,
            Err(UsagePoolStoreError::DuplicateName) => {}
            Err(error) => panic!("unexpected update error: {error}"),
        }
    }
    assert_eq!(successes, 1);
}

// Count writes so compensation cannot delete/recreate identical-looking quota
// definitions after a rename rejection. Definition snapshots alone miss this.
struct CountingRates {
    inner: Arc<dyn RateLimitPolicyStore>,
    writes: AtomicUsize,
}

#[async_trait]
impl RateLimitPolicyStore for CountingRates {
    async fn create(&self, policy: RateLimitPolicy) -> Result<RateLimitPolicy, ServiceError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.create(policy).await
    }
    async fn get(&self, id: &str) -> Result<RateLimitPolicy, ServiceError> {
        self.inner.get(id).await
    }
    async fn list(&self) -> Vec<RateLimitPolicy> {
        self.inner.list().await
    }
    async fn list_page(
        &self,
        before: chrono::DateTime<chrono::Utc>,
        fetch: i64,
    ) -> Result<Vec<RateLimitPolicy>, ServiceError> {
        self.inner.list_page(before, fetch).await
    }
    async fn update(&self, policy: RateLimitPolicy) -> Result<RateLimitPolicy, ServiceError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.update(policy).await
    }
    async fn delete(&self, id: &str) -> Result<(), ServiceError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.delete(id).await
    }
    async fn delete_for_api_key(&self, key_hash: &str) -> Result<(), ServiceError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.delete_for_api_key(key_hash).await
    }
    async fn matching(
        &self,
        request: &RateLimitMatch<'_>,
    ) -> Result<Vec<RateLimitPolicy>, ServiceError> {
        self.inner.matching(request).await
    }
}

fn input(name: &str) -> CreateUsagePoolInput {
    CreateUsagePoolInput {
        name: name.into(),
        usage_budgets: vec![ApiKeyBudgetInput {
            token_limit: 100,
            period: "daily".into(),
            enabled: true,
        }],
        rate_limits: vec![ApiKeyRateLimitInput {
            connection_id: None,
            models: vec![],
            requests_per_minute: Some(10),
            request_burst: None,
            tokens_per_minute: None,
            token_burst: None,
            concurrent_requests: None,
            enabled: true,
        }],
    }
}

async fn budget_snapshot(store: &dyn BudgetPolicyStore) -> Vec<String> {
    let mut rows: Vec<_> = store
        .list()
        .await
        .unwrap()
        .iter()
        .map(|row| format!("{row:?}"))
        .collect();
    rows.sort();
    rows
}

async fn service_suite(
    store: Arc<dyn UsagePoolStore>,
    pool: Option<sqlx::PgPool>,
    environment: String,
) {
    let budgets = budget::from_core_store(pool.clone(), environment.clone());
    let rates = Arc::new(CountingRates {
        inner: ratelimit::from_core_store(pool.clone(), environment.clone()),
        writes: AtomicUsize::new(0),
    });
    let service = UsagePoolService::new(store, budgets.clone(), rates.clone(), environment, true);
    let occupied = service.create(input("Occupied")).await.unwrap();
    let working = service.create(input("Working")).await.unwrap();
    let fetched = service
        .lookup(Some(UsagePoolLookup::Name("\u{3000}Occupied\t".into())))
        .await
        .unwrap();
    assert_eq!(fetched.id, occupied.id);
    for lookup in [
        UsagePoolLookup::Name("occupied".into()),
        UsagePoolLookup::Name(working.id.clone()),
        UsagePoolLookup::Id("Occupied".into()),
    ] {
        assert!(matches!(
            service.lookup(Some(lookup)).await,
            Err(ServiceError::NotFound { .. })
        ));
    }
    for (lookup, field) in [
        (None, "lookup"),
        (Some(UsagePoolLookup::Id(" ".into())), "id"),
        (Some(UsagePoolLookup::Name(String::new())), "name"),
        (Some(UsagePoolLookup::Name(" \t\u{3000}".into())), "name"),
    ] {
        let error = service.lookup(lookup).await.unwrap_err();
        assert!(
            matches!(error, ServiceError::InvalidArgument { field: Some(actual), .. } if actual == field)
        );
    }
    let named_like_id = service
        .create(CreateUsagePoolInput {
            name: working.id.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        service
            .lookup(Some(UsagePoolLookup::Id(working.id.clone())))
            .await
            .unwrap()
            .id,
        working.id
    );
    assert_eq!(
        service
            .lookup(Some(UsagePoolLookup::Name(working.id.clone())))
            .await
            .unwrap()
            .id,
        named_like_id.id
    );
    service.delete(&named_like_id.id).await.unwrap();
    let before_budgets = budget_snapshot(budgets.as_ref()).await;
    let before_rates = serde_json::to_value(rates.list().await).unwrap();
    let before_writes = rates.writes.load(Ordering::SeqCst);
    let updates = [
        UpdateUsagePoolInput {
            name: Some(" Occupied ".into()),
            ..Default::default()
        },
        UpdateUsagePoolInput {
            name: Some("Occupied".into()),
            usage_budgets: Some(vec![]),
            rate_limits: Some(vec![]),
        },
    ];
    for update in updates {
        let error = service.update(&working.id, update).await.unwrap_err();
        assert!(matches!(error, ServiceError::AlreadyExists { .. }));
        assert_eq!(error.http_status(), axum::http::StatusCode::CONFLICT);
        assert_eq!(
            tonic::Status::from(error).code(),
            tonic::Code::AlreadyExists
        );
    }
    assert!(matches!(
        service.create(input("Occupied")).await,
        Err(ServiceError::AlreadyExists { .. })
    ));
    // Also exercise creation without quotas, which takes a separate store path.
    assert!(matches!(
        service
            .create(CreateUsagePoolInput {
                name: "Occupied".into(),
                ..Default::default()
            })
            .await,
        Err(ServiceError::AlreadyExists { .. })
    ));
    assert_eq!(budget_snapshot(budgets.as_ref()).await, before_budgets);
    assert_eq!(
        serde_json::to_value(rates.list().await).unwrap(),
        before_rates
    );
    assert_eq!(rates.writes.load(Ordering::SeqCst), before_writes);
    assert_eq!(service.get(&working.id).await.unwrap().name, "Working");

    let lower = service.create(input("occupied")).await.unwrap();
    let upper = service
        .create(CreateUsagePoolInput {
            name: "OCCUPIED".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    // A case variant of an existing name is allowed through the combined
    // quota transaction too. Whitespace is still trimmed by the service.
    let updated = service
        .update(
            &working.id,
            UpdateUsagePoolInput {
                name: Some("\u{3000}oCCUPIED\t".into()),
                usage_budgets: Some(vec![]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.name, "oCCUPIED");
    assert_eq!(updated.id, working.id);
    assert!(matches!(
        service
            .lookup(Some(UsagePoolLookup::Name("Working".into())))
            .await,
        Err(ServiceError::NotFound { .. })
    ));
    assert_eq!(
        service
            .lookup(Some(UsagePoolLookup::Name("oCCUPIED".into())))
            .await
            .unwrap()
            .id,
        working.id
    );
    service.delete(&working.id).await.unwrap();
    assert!(matches!(
        service
            .lookup(Some(UsagePoolLookup::Name("oCCUPIED".into())))
            .await,
        Err(ServiceError::NotFound { .. })
    ));
    service.delete(&occupied.id).await.unwrap();
    service.delete(&lower.id).await.unwrap();
    service.delete(&upper.id).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_names_conformance() {
    let store = Arc::new(InMemoryUsagePoolStore::new());
    store_suite(store.clone()).await;
    // A separate environment/store may use the same name.
    InMemoryUsagePoolStore::new()
        .create_pool("production".into())
        .await
        .unwrap();
    service_suite(store, None, "pool-names".into()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_names_conformance() {
    let Some(url) = crate::support::db::test_database_url("postgres_names_conformance") else {
        return;
    };
    let _lock = crate::support::db::connect_locked(&url).await;
    openai_server::migrate::run(&url).await.unwrap();
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let environment = format!("pool-names-{}", uuid::Uuid::new_v4());
    let store = Arc::new(PostgresUsagePoolStore::new(
        pool.clone(),
        environment.clone(),
    ));
    store_suite(store.clone()).await;
    let other_store = PostgresUsagePoolStore::new(pool.clone(), format!("{environment}-other"));
    let other_pool = other_store.create_pool("production".into()).await.unwrap();
    assert_eq!(
        other_store
            .get_pool_by_name("production")
            .await
            .unwrap()
            .unwrap()
            .id,
        other_pool.id
    );
    assert_ne!(
        store
            .get_pool_by_name("production")
            .await
            .unwrap()
            .unwrap()
            .id,
        other_pool.id
    );
    other_store
        .create_pool("Only outside".into())
        .await
        .unwrap();
    assert!(store
        .get_pool_by_name("Only outside")
        .await
        .unwrap()
        .is_none());

    // An unrelated unique violation must retain its actual error category.
    let existing = store.list_pools().await.pop().unwrap();
    let error = sqlx::query("INSERT INTO chalk_router.usage_pools (environment_id,id,name) VALUES ($1,$2,'Unique primary key test')")
        .bind(&environment).bind(&existing.id).execute(&pool).await.unwrap_err();
    assert!(matches!(
        UsagePoolStoreError::from(error),
        UsagePoolStoreError::Internal(_)
    ));
    service_suite(store, Some(pool.clone()), environment).await;
    pool.close().await;
}
