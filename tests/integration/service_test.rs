//! Unit tests for the service layer. These exercise the same code paths
//! that both HTTP and gRPC handlers dispatch into, so a regression here
//! is a regression in both wire layers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use openai_server::{
    api_keys::{ApiKey, ApiKeyConnectionScope, ApiKeyPermission, ApiKeyStore, InMemoryApiKeyStore},
    budget::{ApiKeyBudgetInput, BudgetPolicyStore, MemoryBudgetPolicyStore},
    fallback::{FallbackPolicy, InMemoryFallbackPolicyStore},
    hashing::KeyHasher,
    judge_policy::{InMemoryJudgePolicyStore, JudgeLadder, JudgePolicy},
    model_routes::{InMemoryModelRouteStore, ModelRouteStore, RouteInput},
    provider_connections::{
        ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore, ProviderConnectionStore,
    },
    quota::DisabledQuota,
    ratelimit::{
        ApiKeyRateLimitInput, MemoryRateLimitPolicyStore, RateLimitPolicy, RateLimitPolicyStore,
    },
    service::{
        api_keys::{ApiKeyService, CreateKeyInput, UpdateKeyInput},
        fallback::FallbackPolicyService,
        rate_limits::{CreateRateLimitInput, RateLimitService, UpdateRateLimitInput},
        usage_budgets::UsageBudgetService,
        usage_pools::{CreateUsagePoolInput, UpdateUsagePoolInput, UsagePoolService},
        ServiceError,
    },
    usage_pools::InMemoryUsagePoolStore,
};

struct FailingCreateRateLimitStore {
    inner: MemoryRateLimitPolicyStore,
    fail_cleanup: bool,
    created_key_hash: Arc<Mutex<Option<String>>>,
}

#[async_trait]
impl RateLimitPolicyStore for FailingCreateRateLimitStore {
    async fn create(&self, policy: RateLimitPolicy) -> Result<RateLimitPolicy, ServiceError> {
        *self.created_key_hash.lock().unwrap() = Some(policy.scope_id);
        Err(ServiceError::internal("injected rate-limit create failure"))
    }

    async fn list(&self) -> Vec<RateLimitPolicy> {
        self.inner.list().await
    }

    async fn get(&self, id: &str) -> Result<RateLimitPolicy, ServiceError> {
        self.inner.get(id).await
    }

    async fn list_page(
        &self,
        before: chrono::DateTime<chrono::Utc>,
        fetch: i64,
    ) -> Result<Vec<RateLimitPolicy>, ServiceError> {
        self.inner.list_page(before, fetch).await
    }

    async fn update(&self, policy: RateLimitPolicy) -> Result<RateLimitPolicy, ServiceError> {
        self.inner.update(policy).await
    }

    async fn delete(&self, id: &str) -> Result<(), ServiceError> {
        self.inner.delete(id).await
    }

    async fn delete_for_api_key(&self, key_hash: &str) -> Result<(), ServiceError> {
        if self.fail_cleanup {
            Err(ServiceError::internal(
                "injected rate-limit cleanup failure",
            ))
        } else {
            self.inner.delete_for_api_key(key_hash).await
        }
    }

    async fn matching(
        &self,
        request: &openai_server::ratelimit::RateLimitMatch<'_>,
    ) -> Result<Vec<RateLimitPolicy>, ServiceError> {
        self.inner.matching(request).await
    }
}

fn api_key_service() -> ApiKeyService {
    let store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
        b"test-pepper".to_vec(),
    )));
    ApiKeyService::new(
        store,
        None,
        Arc::new(MemoryBudgetPolicyStore::new("test".to_string())),
        Arc::new(MemoryRateLimitPolicyStore::new("test".to_string())),
        std::sync::Arc::new(
            openai_server::provider_connections::InMemoryProviderConnectionStore::new(),
        ),
        std::sync::Arc::new(openai_server::model_routes::InMemoryModelRouteStore::new()),
        false,
    )
}

fn usage_pool_service() -> UsagePoolService {
    UsagePoolService::new(
        Arc::new(InMemoryUsagePoolStore::new()),
        Arc::new(MemoryBudgetPolicyStore::new("test".to_string())),
        Arc::new(MemoryRateLimitPolicyStore::new("test".to_string())),
        "test".to_string(),
        false,
    )
}

fn rate_limit_service() -> RateLimitService {
    RateLimitService::new(
        Arc::new(MemoryRateLimitPolicyStore::new("test".to_string())),
        false,
    )
}

/// A pool service and a budget service sharing the same pool store, so a
/// budget can be scoped to a pool that `validate_definition` will accept.
fn usage_pool_and_budget_services() -> (UsagePoolService, UsageBudgetService) {
    let pool_store = Arc::new(InMemoryUsagePoolStore::new());
    let pool_service = UsagePoolService::new(
        pool_store.clone(),
        Arc::new(MemoryBudgetPolicyStore::new("test".to_string())),
        Arc::new(MemoryRateLimitPolicyStore::new("test".to_string())),
        "test".to_string(),
        false,
    );
    let budget_service = UsageBudgetService::new(
        Arc::new(MemoryBudgetPolicyStore::new("test".to_string())),
        Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
            b"test-pepper".to_vec(),
        ))),
        pool_store,
        "test".to_string(),
        true,
        Arc::new(DisabledQuota),
    );
    (pool_service, budget_service)
}

fn fallback_service() -> FallbackPolicyService {
    FallbackPolicyService::new(Arc::new(InMemoryFallbackPolicyStore::new(
        FallbackPolicy::empty(),
    )))
}

#[tokio::test]
async fn create_then_list_returns_the_new_key() {
    let svc = api_key_service();

    let created = svc
        .create(CreateKeyInput {
            description: Some("test".to_string()),
            permission: Some(
                ApiKeyPermission {
                    connection_scopes: Some(vec![ApiKeyConnectionScope {
                        connection_id: "conn-openai".into(),
                        models: Vec::new(),
                    }]),
                    ..Default::default()
                }
                .into(),
            ),
            ..Default::default()
        })
        .await
        .expect("create");

    assert_eq!(created.api_key.description, "test");
    // The raw secret is returned exactly once, at creation.
    assert!(created.raw_key.starts_with("ck-"));

    let listed = svc.list().await;
    assert_eq!(listed.len(), 1);
    // Stored keys expose only the hash, never the raw secret.
    assert_eq!(listed[0].key_hash, created.api_key.key_hash);
}

#[tokio::test]
async fn api_key_budget_requires_an_enabled_quota_runtime() {
    let svc = api_key_service();
    let error = svc
        .create(CreateKeyInput {
            description: Some("budgeted".to_string()),
            daily_token_budget: Some(100),
            ..Default::default()
        })
        .await
        .expect_err("an unenforceable enabled budget must be rejected");
    assert!(matches!(error, ServiceError::FailedPrecondition { .. }));
}

/// An API-key service over stores the test can seed with connections and
/// routes.
struct KeyFixture {
    keys: Arc<dyn ApiKeyStore>,
    connections: Arc<InMemoryProviderConnectionStore>,
    routes: Arc<InMemoryModelRouteStore>,
    service: ApiKeyService,
}

fn key_fixture() -> KeyFixture {
    let keys: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
        b"test-pepper".to_vec(),
    )));
    let connections = Arc::new(InMemoryProviderConnectionStore::new());
    let routes = Arc::new(InMemoryModelRouteStore::new());
    let service = ApiKeyService::new(
        keys.clone(),
        None,
        Arc::new(MemoryBudgetPolicyStore::new("test".to_string())),
        Arc::new(MemoryRateLimitPolicyStore::new("test".to_string())),
        connections.clone(),
        routes.clone(),
        false,
    );
    KeyFixture {
        keys,
        connections,
        routes,
        service,
    }
}

impl KeyFixture {
    async fn connection(&self, name: &str, kind: &str) -> String {
        self.connections
            .create(ConnectionInput {
                name: name.into(),
                provider_kind: kind.into(),
                base_url: None,
                prefix: None,
                exposure: ExposurePolicy::Dynamic,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: "sk-test".into(),
            })
            .await
            .expect("create connection")
            .id
    }

    async fn route(&self, public_model_id: &str, connection_id: &str) -> String {
        self.routes
            .create(RouteInput {
                public_model_id: public_model_id.into(),
                connection_id: connection_id.into(),
                upstream_model_id: "upstream".into(),
                published: true,
                routing: None,
            })
            .await
            .expect("create route")
            .id
    }

    /// Store `permission` on a new key as an older router wrote it, bypassing
    /// the admin API's validation.
    async fn stored_key(&self, permission: ApiKeyPermission) -> String {
        let created = self
            .service
            .create(CreateKeyInput {
                description: Some("stored".into()),
                ..Default::default()
            })
            .await
            .expect("create key")
            .api_key;
        self.keys
            .insert_raw(ApiKey {
                key_hash: created.key_hash.clone(),
                key_prefix: created.key_prefix.clone(),
                description: created.description.clone(),
                created_at: created.created_at,
                permission,
                labels: HashMap::new(),
                usage_pool_id: None,
                daily_token_budget: None,
            })
            .await
            .expect("replace stored key");
        created.key_hash.clone()
    }
}

fn routes(ids: &[&str]) -> ApiKeyPermission {
    ApiKeyPermission {
        route_ids: Some(ids.iter().map(|id| id.to_string()).collect()),
        ..Default::default()
    }
}

#[tokio::test]
async fn legacy_permission_fields_are_rejected() {
    let f = key_fixture();
    let key = f.stored_key(ApiKeyPermission::default()).await;
    for (field, permission) in [
        (
            "permission.provider",
            ApiKeyPermission {
                provider: Some("openai".into()),
                ..Default::default()
            },
        ),
        (
            "permission.connection_id",
            ApiKeyPermission {
                connection_id: Some("conn".into()),
                ..Default::default()
            },
        ),
        (
            "permission.models",
            ApiKeyPermission {
                models: Some(vec!["openai/gpt-4o".into()]),
                ..Default::default()
            },
        ),
    ] {
        let created = f
            .service
            .create(CreateKeyInput {
                description: Some("legacy".into()),
                permission: Some(permission.clone().into()),
                ..Default::default()
            })
            .await
            .expect_err("a legacy field must not be ignored");
        assert!(
            matches!(&created, ServiceError::InvalidArgument { field: Some(f), .. } if f == field),
            "{created:?}"
        );
        let updated = f
            .service
            .update(
                &key,
                UpdateKeyInput {
                    permission: Some(permission.into()),
                    ..Default::default()
                },
            )
            .await
            .expect_err("a legacy field must not be ignored");
        assert!(matches!(updated, ServiceError::InvalidArgument { .. }));
    }
}

#[tokio::test]
async fn connection_scopes_cannot_grant_model_routes() {
    let f = key_fixture();
    let error = f
        .service
        .create(CreateKeyInput {
            description: Some("route in scope".into()),
            permission: Some(
                ApiKeyPermission {
                    connection_scopes: Some(vec![ApiKeyConnectionScope {
                        connection_id: "conn".into(),
                        models: vec!["chalk/assistant".into()],
                    }]),
                    ..Default::default()
                }
                .into(),
            ),
            ..Default::default()
        })
        .await
        .expect_err("a route id in a connection scope grants nothing");
    assert!(
        matches!(&error, ServiceError::InvalidArgument { field: Some(f), .. }
            if f == "permission.connection_scopes[0].models"),
        "{error:?}"
    );
}

#[tokio::test]
async fn route_grants_must_name_existing_routes() {
    let f = key_fixture();
    let connection = f.connection("primary", "openai").await;
    let assistant = f.route("chalk/assistant", &connection).await;
    let error = f
        .service
        .create(CreateKeyInput {
            description: Some("unknown route".into()),
            permission: Some(routes(&[&assistant, "missing-route"]).into()),
            ..Default::default()
        })
        .await
        .expect_err("an unknown route id is rejected");
    assert!(
        matches!(&error, ServiceError::InvalidArgument { field: Some(f), .. }
            if f == "permission.routes.refs[1].route_id"),
        "{error:?}"
    );

    let created = f
        .service
        .create(CreateKeyInput {
            description: Some("granted".into()),
            permission: Some(routes(&[&assistant, &assistant]).into()),
            ..Default::default()
        })
        .await
        .expect("an existing route is granted")
        .api_key;
    assert_eq!(
        created.permission.route_ids.as_deref(),
        Some([assistant.clone()].as_slice())
    );

    // A grant whose route was deleted stays on the key, denying nothing new,
    // so an unrelated edit of the key does not fail on it.
    let dangling = f.stored_key(routes(&["deleted-route"])).await;
    let updated = f
        .service
        .update(
            &dangling,
            UpdateKeyInput {
                permission: Some(routes(&["deleted-route", &assistant]).into()),
                ..Default::default()
            },
        )
        .await
        .expect("a grant the key already carries is kept");
    assert_eq!(
        updated.permission.route_ids,
        Some(vec!["deleted-route".to_string(), assistant])
    );
}

#[tokio::test]
async fn admin_reads_restate_legacy_fields_as_connection_scopes() {
    let f = key_fixture();
    let openai = f.connection("openai-a", "openai").await;
    let other_openai = f.connection("openai-b", "openai").await;
    let anthropic = f.connection("anthropic", "anthropic").await;
    let key = f
        .stored_key(ApiKeyPermission {
            provider: Some("openai".into()),
            models: Some(vec!["chalk/assistant".into(), "openai/gpt-4o".into()]),
            route_ids: Some(vec!["route-1".into()]),
            ..Default::default()
        })
        .await;

    let presented = f
        .service
        .get_with_usage(&key)
        .await
        .expect("read key")
        .api_key;
    let permission = &presented.permission;
    assert!(!permission.has_legacy_fields());
    let scopes = permission.connection_scopes.as_ref().expect("restated");
    let scoped: Vec<_> = scopes
        .iter()
        .map(|scope| (scope.connection_id.as_str(), scope.models.as_slice()))
        .collect();
    let gpt = ["openai/gpt-4o".to_string()];
    assert_eq!(
        scoped,
        [
            (openai.as_str(), &gpt[..]),
            (other_openai.as_str(), &gpt[..])
        ]
    );
    assert!(!scoped.iter().any(|(id, _)| *id == anthropic));
    assert_eq!(permission.route_ids, Some(vec!["route-1".to_string()]));

    // The stored key and what it may call are unchanged.
    let stored = f.keys.get(&key).await.unwrap();
    assert_eq!(stored.permission.provider.as_deref(), Some("openai"));
    for (connection, kind, model, allowed) in [
        (&openai, "openai", "openai/gpt-4o", true),
        (&anthropic, "anthropic", "openai/gpt-4o", false),
        (&openai, "openai", "openai/gpt-5", false),
    ] {
        assert_eq!(
            stored
                .permission
                .allows(kind, Some(connection), model)
                .is_ok(),
            allowed
        );
        assert_eq!(
            permission.allows(kind, Some(connection), model).is_ok(),
            allowed
        );
    }

    // Saving the presented permission back is accepted.
    let saved = f
        .service
        .update(
            &key,
            UpdateKeyInput {
                permission: Some(presented.permission.clone().into()),
                ..Default::default()
            },
        )
        .await
        .expect("the admin view saves back");
    assert!(!saved.permission.has_legacy_fields());
}

/// An unreadable stored permission is kept as written, and denies the key,
/// until the permission is replaced.
#[tokio::test]
async fn unreadable_permission_is_kept_until_replaced() {
    let f = key_fixture();
    let key = f.stored_key(ApiKeyPermission::unreadable()).await;
    let renamed = f
        .service
        .update(
            &key,
            UpdateKeyInput {
                description: Some("renamed".into()),
                ..Default::default()
            },
        )
        .await
        .expect("updates that leave the permission alone still apply");
    assert!(renamed.permission.unreadable);
    assert!(renamed
        .permission
        .allows("openai", Some("conn-1"), "gpt-4o")
        .is_err());

    let replaced = f
        .service
        .update(
            &key,
            UpdateKeyInput {
                permission: Some(routes(&[]).into()),
                ..Default::default()
            },
        )
        .await
        .expect("a permission replaces an unreadable one");
    assert!(!replaced.permission.unreadable);
    assert!(replaced.permission.is_restricted());
}

#[tokio::test]
async fn api_key_budget_must_be_positive() {
    let store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
        b"test-pepper".to_vec(),
    )));
    let svc = ApiKeyService::new(
        store,
        None,
        Arc::new(MemoryBudgetPolicyStore::new("test".to_string())),
        Arc::new(MemoryRateLimitPolicyStore::new("test".to_string())),
        std::sync::Arc::new(
            openai_server::provider_connections::InMemoryProviderConnectionStore::new(),
        ),
        std::sync::Arc::new(openai_server::model_routes::InMemoryModelRouteStore::new()),
        true,
    );
    let error = svc
        .create(CreateKeyInput {
            description: Some("budgeted".to_string()),
            daily_token_budget: Some(0),
            ..Default::default()
        })
        .await
        .expect_err("zero is not a usable hard budget");
    assert!(matches!(error, ServiceError::InvalidArgument { .. }));
}

#[tokio::test]
async fn api_key_creation_expands_scoped_rate_limits_and_revoke_cleans_them_up() {
    let store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
        b"test-pepper".to_vec(),
    )));
    let rate_limits = Arc::new(MemoryRateLimitPolicyStore::new("test".to_string()));
    let service = ApiKeyService::new(
        store,
        None,
        Arc::new(MemoryBudgetPolicyStore::new("test".to_string())),
        rate_limits.clone(),
        std::sync::Arc::new(
            openai_server::provider_connections::InMemoryProviderConnectionStore::new(),
        ),
        std::sync::Arc::new(openai_server::model_routes::InMemoryModelRouteStore::new()),
        true,
    );
    let created = service
        .create(CreateKeyInput {
            description: Some("limited key".to_string()),
            permission: Some(
                ApiKeyPermission {
                    connection_scopes: Some(vec![ApiKeyConnectionScope {
                        connection_id: "conn-1".to_string(),
                        models: vec!["gpt-4o".to_string()],
                    }]),
                    ..Default::default()
                }
                .into(),
            ),
            rate_limits: vec![ApiKeyRateLimitInput {
                connection_id: Some(" conn-1 ".to_string()),
                models: vec!["gpt-4o".to_string(), " gpt-4o ".to_string()],
                requests_per_minute: Some(60),
                request_burst: Some(10),
                tokens_per_minute: Some(100_000),
                token_burst: None,
                concurrent_requests: Some(5),
                enabled: false,
            }],
            ..Default::default()
        })
        .await
        .expect("create limited key");

    let policies = rate_limits.list().await;
    assert_eq!(policies.len(), 3);
    assert!(policies.iter().all(|policy| {
        policy.scope == "api_key"
            && policy.scope_id == created.api_key.key_hash
            && policy.connection_id.as_deref() == Some("conn-1")
            && policy.models == ["gpt-4o"]
            && !policy.enabled
    }));
    let listed = service
        .list_with_usage()
        .await
        .expect("list key with direct rate limits");
    assert_eq!(listed.len(), 1);
    assert!(listed[0].rate_limits.iter().all(|limit| !limit.enabled));

    service
        .revoke(&created.api_key.key_hash)
        .await
        .expect("revoke key");
    assert!(rate_limits.list().await.is_empty());
}

#[tokio::test]
async fn duplicate_direct_api_key_rate_limits_are_rejected_before_creation() {
    let store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
        b"test-pepper".to_vec(),
    )));
    let rate_limits = Arc::new(MemoryRateLimitPolicyStore::new("test".to_string()));
    let service = ApiKeyService::new(
        store,
        None,
        Arc::new(MemoryBudgetPolicyStore::new("test".to_string())),
        rate_limits.clone(),
        std::sync::Arc::new(
            openai_server::provider_connections::InMemoryProviderConnectionStore::new(),
        ),
        std::sync::Arc::new(openai_server::model_routes::InMemoryModelRouteStore::new()),
        true,
    );
    let make_limit = |models: Vec<String>, enabled| ApiKeyRateLimitInput {
        connection_id: Some("conn-1".to_string()),
        models,
        requests_per_minute: Some(60),
        request_burst: None,
        tokens_per_minute: None,
        token_burst: None,
        concurrent_requests: None,
        enabled,
    };

    let error = service
        .create(CreateKeyInput {
            description: Some("duplicate direct limits".to_string()),
            rate_limits: vec![
                make_limit(vec!["model-b".into(), "model-a".into()], true),
                make_limit(vec!["model-a".into(), "model-b".into()], false),
            ],
            ..Default::default()
        })
        .await
        .expect_err("equivalent direct policy drafts must conflict");

    assert!(matches!(error, ServiceError::InvalidArgument { .. }));
    assert!(error
        .to_string()
        .contains("Edit the existing rate limit instead"));
    assert!(service.list().await.is_empty());
    assert!(rate_limits.list().await.is_empty());
}

#[tokio::test]
async fn duplicate_direct_api_key_replacement_is_rejected_before_mutation() {
    let store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
        b"test-pepper".to_vec(),
    )));
    let rate_limits = Arc::new(MemoryRateLimitPolicyStore::new("test".to_string()));
    let service = ApiKeyService::new(
        store,
        None,
        Arc::new(MemoryBudgetPolicyStore::new("test".to_string())),
        rate_limits.clone(),
        std::sync::Arc::new(
            openai_server::provider_connections::InMemoryProviderConnectionStore::new(),
        ),
        std::sync::Arc::new(openai_server::model_routes::InMemoryModelRouteStore::new()),
        true,
    );
    let initial = ApiKeyRateLimitInput {
        connection_id: Some("conn-1".to_string()),
        models: vec!["model-old".to_string()],
        requests_per_minute: Some(30),
        request_burst: None,
        tokens_per_minute: None,
        token_burst: None,
        concurrent_requests: None,
        enabled: true,
    };
    let created = service
        .create(CreateKeyInput {
            description: Some("unchanged".to_string()),
            rate_limits: vec![initial],
            ..Default::default()
        })
        .await
        .expect("create key with direct limit");
    let duplicate = ApiKeyRateLimitInput {
        connection_id: Some("conn-1".to_string()),
        models: vec!["model-new".to_string()],
        requests_per_minute: Some(60),
        request_burst: None,
        tokens_per_minute: None,
        token_burst: None,
        concurrent_requests: None,
        enabled: true,
    };

    service
        .update(
            &created.api_key.key_hash,
            UpdateKeyInput {
                description: Some("must not persist".to_string()),
                rate_limits: Some(vec![duplicate.clone(), duplicate]),
                ..Default::default()
            },
        )
        .await
        .expect_err("duplicate replacement must fail before changing the key or policies");

    assert_eq!(
        service
            .get(&created.api_key.key_hash)
            .await
            .expect("key remains")
            .description,
        "unchanged"
    );
    let policies = rate_limits.list().await;
    assert_eq!(policies.len(), 1);
    assert_eq!(policies[0].models, ["model-old"]);
    assert_eq!(policies[0].limit_value, 30);
}

#[tokio::test]
async fn failed_rate_limit_provisioning_revokes_the_key_and_clears_its_budget() {
    let store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
        b"test-pepper".to_vec(),
    )));
    let budget = Arc::new(MemoryBudgetPolicyStore::new("test".to_string()));
    let created_key_hash = Arc::new(Mutex::new(None));
    let service = ApiKeyService::new(
        store,
        None,
        budget.clone(),
        Arc::new(FailingCreateRateLimitStore {
            inner: MemoryRateLimitPolicyStore::new("test".to_string()),
            fail_cleanup: false,
            created_key_hash: created_key_hash.clone(),
        }),
        std::sync::Arc::new(
            openai_server::provider_connections::InMemoryProviderConnectionStore::new(),
        ),
        std::sync::Arc::new(openai_server::model_routes::InMemoryModelRouteStore::new()),
        true,
    );

    let error = service
        .create(CreateKeyInput {
            description: Some("rollback key".to_string()),
            daily_token_budget: Some(1_000),
            rate_limits: vec![ApiKeyRateLimitInput {
                connection_id: None,
                models: Vec::new(),
                requests_per_minute: Some(60),
                request_burst: None,
                tokens_per_minute: None,
                token_burst: None,
                concurrent_requests: None,
                enabled: true,
            }],
            ..Default::default()
        })
        .await
        .expect_err("rate-limit provisioning should fail");

    assert!(error
        .to_string()
        .contains("injected rate-limit create failure"));
    assert!(service.list().await.is_empty());
    let key_hash = created_key_hash.lock().unwrap().clone().unwrap();
    assert!(budget.matching(&key_hash, None).await.unwrap().is_empty());
}

#[tokio::test]
async fn create_rollback_surfaces_cleanup_failures_after_revoking_the_key() {
    let store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
        b"test-pepper".to_vec(),
    )));
    let created_key_hash = Arc::new(Mutex::new(None));
    let service = ApiKeyService::new(
        store,
        None,
        Arc::new(MemoryBudgetPolicyStore::new("test".to_string())),
        Arc::new(FailingCreateRateLimitStore {
            inner: MemoryRateLimitPolicyStore::new("test".to_string()),
            fail_cleanup: true,
            created_key_hash,
        }),
        std::sync::Arc::new(
            openai_server::provider_connections::InMemoryProviderConnectionStore::new(),
        ),
        std::sync::Arc::new(openai_server::model_routes::InMemoryModelRouteStore::new()),
        true,
    );

    let error = service
        .create(CreateKeyInput {
            description: Some("observable rollback".to_string()),
            rate_limits: vec![ApiKeyRateLimitInput {
                connection_id: None,
                models: Vec::new(),
                requests_per_minute: Some(60),
                request_burst: None,
                tokens_per_minute: None,
                token_burst: None,
                concurrent_requests: None,
                enabled: true,
            }],
            ..Default::default()
        })
        .await
        .expect_err("rate-limit provisioning should fail");

    assert!(error.to_string().contains("rollback was incomplete"));
    assert!(error.to_string().contains("rate-limit cleanup"));
    assert!(service.list().await.is_empty());
}

#[tokio::test]
async fn api_key_rate_limit_selector_must_be_inside_permission_scope() {
    let store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
        b"test-pepper".to_vec(),
    )));
    let rate_limits = Arc::new(MemoryRateLimitPolicyStore::new("test".to_string()));
    let service = ApiKeyService::new(
        store,
        None,
        Arc::new(MemoryBudgetPolicyStore::new("test".to_string())),
        rate_limits.clone(),
        std::sync::Arc::new(
            openai_server::provider_connections::InMemoryProviderConnectionStore::new(),
        ),
        std::sync::Arc::new(openai_server::model_routes::InMemoryModelRouteStore::new()),
        true,
    );
    let error = service
        .create(CreateKeyInput {
            description: Some("invalid limited key".to_string()),
            permission: Some(
                ApiKeyPermission {
                    connection_scopes: Some(vec![ApiKeyConnectionScope {
                        connection_id: "conn-1".to_string(),
                        models: vec!["gpt-4o".to_string()],
                    }]),
                    ..Default::default()
                }
                .into(),
            ),
            rate_limits: vec![ApiKeyRateLimitInput {
                connection_id: Some("conn-1".to_string()),
                models: Vec::new(),
                requests_per_minute: Some(60),
                request_burst: None,
                tokens_per_minute: None,
                token_burst: None,
                concurrent_requests: None,
                enabled: true,
            }],
            ..Default::default()
        })
        .await
        .expect_err("all-model target exceeds the key's one-model permission");

    assert!(matches!(error, ServiceError::InvalidArgument { .. }));
    assert!(service.list().await.is_empty());
    assert!(rate_limits.list().await.is_empty());
}

#[tokio::test]
async fn api_key_labels_are_validated_and_immutable() {
    let svc = api_key_service();
    let labels = HashMap::from([
        ("environment".to_string(), "production".to_string()),
        ("team.name".to_string(), "routing".to_string()),
    ]);
    let created = svc
        .create(CreateKeyInput {
            description: Some("labeled".into()),
            labels: Some(labels.clone()),
            ..Default::default()
        })
        .await
        .expect("create labeled key");
    assert_eq!(created.api_key.labels, labels);

    let updated = svc
        .update(
            &created.api_key.key_hash,
            UpdateKeyInput {
                description: Some("renamed".into()),
                ..Default::default()
            },
        )
        .await
        .expect("update mutable metadata");
    assert_eq!(updated.labels, labels);

    let error = svc
        .create(CreateKeyInput {
            description: Some("invalid labels".into()),
            labels: Some(HashMap::from([("Team".into(), "routing".into())])),
            ..Default::default()
        })
        .await
        .expect_err("invalid labels must fail");
    assert!(
        matches!(error, ServiceError::InvalidArgument { field: Some(ref field), .. } if field == "labels")
    );
}

#[tokio::test]
async fn api_key_permission_can_be_replaced_without_changing_labels() {
    let svc = api_key_service();
    let labels = HashMap::from([("team".to_string(), "routing".to_string())]);
    let created = svc
        .create(CreateKeyInput {
            description: Some("scoped".into()),
            labels: Some(labels.clone()),
            ..Default::default()
        })
        .await
        .unwrap();

    let permission = ApiKeyPermission {
        connection_scopes: Some(vec![openai_server::api_keys::ApiKeyConnectionScope {
            connection_id: "connection-1".to_string(),
            models: vec!["openai/gpt-4o".to_string()],
        }]),
        downgradable: Some(false),
        ..Default::default()
    };
    let updated = svc
        .update(
            &created.api_key.key_hash,
            UpdateKeyInput {
                permission: Some(permission.clone().into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let scopes = updated.permission.connection_scopes.as_ref().unwrap();
    assert_eq!(scopes.len(), 1);
    assert_eq!(scopes[0].connection_id, "connection-1");
    assert_eq!(scopes[0].models, ["openai/gpt-4o"]);
    assert_eq!(updated.permission.downgradable, Some(false));
    assert_eq!(updated.labels, labels);
}

#[tokio::test]
async fn deprecated_cost_tags_are_rejected() {
    let svc = api_key_service();
    for cost_tags in [
        HashMap::new(),
        HashMap::from([("team".into(), "routing".into())]),
    ] {
        let error = svc
            .create(CreateKeyInput {
                description: Some("deprecated input".into()),
                cost_tags: Some(cost_tags),
                ..Default::default()
            })
            .await
            .expect_err("present cost_tags must fail");
        assert!(
            matches!(error, ServiceError::InvalidArgument { field: Some(ref field), .. } if field == "cost_tags")
        );
    }

    let created = svc
        .create(CreateKeyInput {
            description: Some("created".into()),
            ..Default::default()
        })
        .await
        .expect("create key");
    let error = svc
        .update(
            &created.api_key.key_hash,
            UpdateKeyInput {
                cost_tags: Some(HashMap::new()),
                ..Default::default()
            },
        )
        .await
        .expect_err("even a clear attempt must fail");
    assert!(
        matches!(error, ServiceError::InvalidArgument { field: Some(ref field), .. } if field == "cost_tags")
    );
}

#[tokio::test]
async fn api_key_descriptions_are_required_normalized_and_bounded() {
    let svc = api_key_service();

    for description in [
        None,
        Some("   ".to_string()),
        Some("line\nbreak".to_string()),
    ] {
        let err = svc
            .create(CreateKeyInput {
                description,
                ..Default::default()
            })
            .await
            .expect_err("description must be valid");
        assert!(
            matches!(err, ServiceError::InvalidArgument { field: Some(ref field), .. } if field == "description")
        );
    }

    let err = svc
        .create(CreateKeyInput {
            description: Some("x".repeat(121)),
            ..Default::default()
        })
        .await
        .expect_err("description should have a maximum length");
    assert!(
        matches!(err, ServiceError::InvalidArgument { field: Some(ref field), .. } if field == "description")
    );

    let created = svc
        .create(CreateKeyInput {
            description: Some("  Clé de déploiement  ".into()),
            ..Default::default()
        })
        .await
        .expect("valid Unicode description");
    assert_eq!(created.api_key.description, "Clé de déploiement");

    let err = svc
        .update(
            &created.api_key.key_hash,
            UpdateKeyInput {
                description: Some("   ".into()),
                ..Default::default()
            },
        )
        .await
        .expect_err("updates must not clear descriptions");
    assert!(
        matches!(err, ServiceError::InvalidArgument { field: Some(ref field), .. } if field == "description")
    );
}

#[tokio::test]
async fn api_key_list_page_walks_all_keys_without_duplicates() {
    let svc = api_key_service();
    let mut created = Vec::new();
    for i in 0..5 {
        created.push(
            svc.create(CreateKeyInput {
                description: Some(format!("key-{i}")),
                ..Default::default()
            })
            .await
            .expect("create"),
        );
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }

    let (first, cursor) = svc.list_page(Some(2), None).await.expect("first page");
    assert_eq!(first.len(), 2);
    let cursor = cursor.expect("more keys remain");
    let (second, cursor) = svc
        .list_page(Some(2), Some(cursor))
        .await
        .expect("second page");
    assert_eq!(second.len(), 2);
    let cursor = cursor.expect("one key remains");
    let (third, cursor) = svc
        .list_page(Some(2), Some(cursor))
        .await
        .expect("third page");
    assert_eq!(third.len(), 1);
    assert!(cursor.is_none());

    let mut seen: Vec<_> = first
        .into_iter()
        .chain(second)
        .chain(third)
        .map(|key| key.key_hash.clone())
        .collect();
    seen.sort();
    let mut expected: Vec<_> = created
        .into_iter()
        .map(|key| key.api_key.key_hash.clone())
        .collect();
    expected.sort();
    assert_eq!(seen, expected);
}

#[tokio::test]
async fn api_key_list_page_rejects_invalid_cursor_and_limit() {
    let svc = api_key_service();
    let cursor_err = svc
        .list_page(None, Some("not-a-cursor".into()))
        .await
        .unwrap_err();
    assert!(
        matches!(cursor_err, ServiceError::InvalidArgument { field: Some(ref field), .. } if field == "cursor")
    );

    let limit_err = svc.list_page(Some(0), None).await.unwrap_err();
    assert!(
        matches!(limit_err, ServiceError::InvalidArgument { field: Some(ref field), .. } if field == "limit")
    );
}

#[tokio::test]
async fn revoke_unknown_key_is_idempotent() {
    let svc = api_key_service();
    svc.revoke("does-not-exist")
        .await
        .expect("idempotent revoke");
}

#[tokio::test]
async fn update_can_clear_optional_fields() {
    let svc = api_key_service();
    let created = svc
        .create(CreateKeyInput {
            description: Some("orig".into()),
            usage_pool_id: Some("11111111-1111-1111-1111-111111111111".into()),
            ..Default::default()
        })
        .await
        .expect("create");

    // Some(None) means "clear this field", Some(Some(v)) means "set to v".
    // Budgets are disabled in phase 1, so clearing (Some(None)) is a no-op and
    // is allowed; setting one would be rejected.
    let updated = svc
        .update(
            &created.api_key.key_hash,
            UpdateKeyInput {
                description: Some("new".into()),
                daily_token_budget: Some(None),
                usage_pool_id: Some(None),
                ..Default::default()
            },
        )
        .await
        .expect("update");

    assert_eq!(updated.description, "new");
    assert_eq!(updated.usage_pool_id, None);
    assert_eq!(updated.daily_token_budget, None);
}

#[tokio::test]
async fn get_usage_for_unknown_key_returns_not_found() {
    let svc = api_key_service();
    let err = svc.get_usage("missing").await.unwrap_err();
    assert!(matches!(err, ServiceError::NotFound { .. }));
}

#[tokio::test]
async fn disabled_usage_backend_returns_zero_snapshots_on_every_key_read() {
    let svc = api_key_service();
    let created = svc
        .create(CreateKeyInput {
            description: Some("unused".into()),
            ..Default::default()
        })
        .await
        .expect("create");

    let detail = svc
        .get_usage(&created.api_key.key_hash)
        .await
        .expect("get usage");
    assert_eq!(detail.usage.total_tokens, 0);

    let all = svc.list_usage().await.expect("list usage");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].usage.total_tokens, 0);

    let (listed, cursor) = svc
        .list_page_with_usage(Some(10), None)
        .await
        .expect("list keys with usage");
    assert!(cursor.is_none());
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].usage.total_tokens, 0);
}

#[tokio::test]
async fn usage_pool_create_rejects_empty_name() {
    let svc = usage_pool_service();
    let err = svc
        .create(CreateUsagePoolInput {
            name: "   ".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ServiceError::InvalidArgument { .. }));
    assert_eq!(err.grpc_code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn usage_pool_create_rejects_quotas_when_enforcement_is_disabled() {
    let svc = usage_pool_service();
    let err = svc
        .create(CreateUsagePoolInput {
            name: "pool".into(),
            usage_budgets: vec![ApiKeyBudgetInput {
                token_limit: 100,
                period: "daily".into(),
                enabled: true,
            }],
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ServiceError::FailedPrecondition { .. }));
    assert!(svc.list().await.is_empty());
}

#[tokio::test]
async fn usage_pool_create_provisions_and_delete_cleans_shared_quotas() {
    let pool_store = Arc::new(InMemoryUsagePoolStore::new());
    let budget_store = Arc::new(MemoryBudgetPolicyStore::new("test".to_string()));
    let rate_limit_store = Arc::new(MemoryRateLimitPolicyStore::new("test".to_string()));
    let svc = UsagePoolService::new(
        pool_store,
        budget_store.clone(),
        rate_limit_store.clone(),
        "test".to_string(),
        true,
    );

    let pool = svc
        .create(CreateUsagePoolInput {
            name: "shared".into(),
            usage_budgets: vec![ApiKeyBudgetInput {
                token_limit: 100,
                period: "daily".into(),
                enabled: true,
            }],
            rate_limits: vec![ApiKeyRateLimitInput {
                connection_id: None,
                models: Vec::new(),
                requests_per_minute: Some(10),
                request_burst: None,
                tokens_per_minute: None,
                token_burst: None,
                concurrent_requests: Some(2),
                enabled: true,
            }],
        })
        .await
        .expect("create pool and quotas");

    let budgets = budget_store.list().await.expect("list budgets");
    assert_eq!(budgets.len(), 1);
    assert_eq!(budgets[0].scope, "usage_pool");
    assert_eq!(budgets[0].scope_id, pool.id);
    let rate_limits = rate_limit_store.list().await;
    assert_eq!(rate_limits.len(), 2);
    assert!(rate_limits
        .iter()
        .all(|policy| policy.scope == "usage_pool" && policy.scope_id == pool.id));

    svc.delete(&pool.id).await.expect("delete pool");
    assert!(svc.list().await.is_empty());
    assert!(budget_store.list().await.expect("list budgets").is_empty());
    assert!(rate_limit_store.list().await.is_empty());
}

#[tokio::test]
async fn usage_pool_create_rolls_back_when_a_quota_write_fails() {
    let pool_store = Arc::new(InMemoryUsagePoolStore::new());
    let budget_store = Arc::new(MemoryBudgetPolicyStore::new("test".to_string()));
    let rate_limit_store = Arc::new(FailingCreateRateLimitStore {
        inner: MemoryRateLimitPolicyStore::new("test".to_string()),
        fail_cleanup: false,
        created_key_hash: Arc::new(Mutex::new(None)),
    });
    let svc = UsagePoolService::new(
        pool_store,
        budget_store.clone(),
        rate_limit_store,
        "test".to_string(),
        true,
    );

    let err = svc
        .create(CreateUsagePoolInput {
            name: "shared".into(),
            usage_budgets: vec![ApiKeyBudgetInput {
                token_limit: 100,
                period: "daily".into(),
                enabled: true,
            }],
            rate_limits: vec![ApiKeyRateLimitInput {
                connection_id: None,
                models: Vec::new(),
                requests_per_minute: Some(10),
                request_burst: None,
                tokens_per_minute: None,
                token_burst: None,
                concurrent_requests: None,
                enabled: true,
            }],
        })
        .await
        .unwrap_err();

    assert!(matches!(err, ServiceError::Internal { .. }));
    assert!(svc.list().await.is_empty());
    assert!(budget_store.list().await.expect("list budgets").is_empty());
}

#[tokio::test]
async fn usage_pool_update_replaces_selected_quota_sets_and_name() {
    let pool_store = Arc::new(InMemoryUsagePoolStore::new());
    let budget_store = Arc::new(MemoryBudgetPolicyStore::new("test".to_string()));
    let rate_limit_store = Arc::new(MemoryRateLimitPolicyStore::new("test".to_string()));
    let svc = UsagePoolService::new(
        pool_store,
        budget_store.clone(),
        rate_limit_store.clone(),
        "test".to_string(),
        true,
    );
    let pool = svc
        .create(CreateUsagePoolInput {
            name: "before".into(),
            usage_budgets: vec![ApiKeyBudgetInput {
                token_limit: 100,
                period: "daily".into(),
                enabled: true,
            }],
            rate_limits: vec![ApiKeyRateLimitInput {
                connection_id: None,
                models: Vec::new(),
                requests_per_minute: Some(10),
                request_burst: None,
                tokens_per_minute: None,
                token_burst: None,
                concurrent_requests: None,
                enabled: true,
            }],
        })
        .await
        .expect("create");

    let updated = svc
        .update(
            &pool.id,
            UpdateUsagePoolInput {
                name: Some("after".into()),
                usage_budgets: Some(Vec::new()),
                rate_limits: Some(vec![ApiKeyRateLimitInput {
                    connection_id: None,
                    models: Vec::new(),
                    requests_per_minute: None,
                    request_burst: None,
                    tokens_per_minute: None,
                    token_burst: None,
                    concurrent_requests: Some(3),
                    enabled: true,
                }]),
            },
        )
        .await
        .expect("update");

    assert_eq!(updated.name, "after");
    assert!(budget_store.list().await.expect("list budgets").is_empty());
    let limits = rate_limit_store.list().await;
    assert_eq!(limits.len(), 1);
    assert_eq!(limits[0].limit_type, "concurrent_requests");
    assert_eq!(limits[0].limit_value, 3);
}

#[tokio::test]
async fn usage_pool_update_rolls_back_name_and_quotas_after_failure() {
    let pool_store = Arc::new(InMemoryUsagePoolStore::new());
    let budget_store = Arc::new(MemoryBudgetPolicyStore::new("test".to_string()));
    let rate_limit_store = Arc::new(FailingCreateRateLimitStore {
        inner: MemoryRateLimitPolicyStore::new("test".to_string()),
        fail_cleanup: false,
        created_key_hash: Arc::new(Mutex::new(None)),
    });
    let svc = UsagePoolService::new(
        pool_store,
        budget_store.clone(),
        rate_limit_store,
        "test".to_string(),
        true,
    );
    let pool = svc
        .create(CreateUsagePoolInput {
            name: "before".into(),
            usage_budgets: vec![ApiKeyBudgetInput {
                token_limit: 100,
                period: "daily".into(),
                enabled: true,
            }],
            ..Default::default()
        })
        .await
        .expect("create");

    let err = svc
        .update(
            &pool.id,
            UpdateUsagePoolInput {
                name: Some("after".into()),
                usage_budgets: Some(vec![ApiKeyBudgetInput {
                    token_limit: 200,
                    period: "daily".into(),
                    enabled: true,
                }]),
                rate_limits: Some(vec![ApiKeyRateLimitInput {
                    connection_id: None,
                    models: Vec::new(),
                    requests_per_minute: Some(10),
                    request_burst: None,
                    tokens_per_minute: None,
                    token_burst: None,
                    concurrent_requests: None,
                    enabled: true,
                }]),
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(err, ServiceError::Internal { .. }));
    assert_eq!(svc.list().await[0].name, "before");
    let budgets = budget_store.list().await.expect("list budgets");
    assert_eq!(budgets.len(), 1);
    assert_eq!(budgets[0].limit_amount, 100);
}

#[tokio::test]
async fn usage_pool_get_returns_created_pool() {
    let svc = usage_pool_service();
    let created = svc
        .create(CreateUsagePoolInput {
            name: "pool".into(),
            ..Default::default()
        })
        .await
        .expect("create");

    let fetched = svc.get(&created.id).await.expect("get");
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.name, "pool");
}

#[tokio::test]
async fn usage_pool_get_unknown_is_not_found() {
    let svc = usage_pool_service();
    let err = svc.get("nope").await.unwrap_err();
    assert!(matches!(err, ServiceError::NotFound { .. }));
}

#[tokio::test]
async fn usage_pool_get_rejects_empty_id() {
    let svc = usage_pool_service();
    let err = svc.get("   ").await.unwrap_err();
    assert!(matches!(err, ServiceError::InvalidArgument { .. }));
}

#[tokio::test]
async fn usage_pool_delete_unknown_is_idempotent() {
    let svc = usage_pool_service();
    svc.delete("nope").await.expect("idempotent delete");
}

#[tokio::test]
async fn usage_pool_list_page_rejects_invalid_cursor_and_limit() {
    let svc = usage_pool_service();

    let limit_err = svc.list_page(Some(0), None).await.unwrap_err();
    assert!(
        matches!(limit_err, ServiceError::InvalidArgument { field: Some(ref f), .. } if f == "limit")
    );

    let cursor_err = svc
        .list_page(None, Some("not-a-timestamp".into()))
        .await
        .unwrap_err();
    assert!(
        matches!(cursor_err, ServiceError::InvalidArgument { field: Some(ref f), .. } if f == "cursor")
    );
}

#[tokio::test]
async fn usage_pool_list_page_walks_all_pages_newest_first() {
    let svc = usage_pool_service();

    // Create 5 pools with strictly increasing `created_at`. A short sleep
    // between creates guarantees distinct timestamps regardless of the
    // clock's resolution.
    let mut created = Vec::new();
    for i in 0..5 {
        created.push(
            svc.create(CreateUsagePoolInput {
                name: format!("pool-{i}"),
                ..Default::default()
            })
            .await
            .expect("create"),
        );
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    // `list_page` orders newest-first, i.e. the reverse of creation order.
    let mut expected_order: Vec<String> = created.iter().map(|p| p.id.clone()).collect();
    expected_order.reverse();

    // Page 1: 2 rows, more remain.
    let (page1, cursor1) = svc.list_page(Some(2), None).await.expect("page1");
    assert_eq!(
        page1.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
        expected_order[0..2]
    );
    let cursor1 = cursor1.expect("2 more pools remain after the first page");

    // Page 2: 2 more rows, exactly one remains.
    let (page2, cursor2) = svc.list_page(Some(2), Some(cursor1)).await.expect("page2");
    assert_eq!(
        page2.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
        expected_order[2..4]
    );
    let cursor2 = cursor2.expect("1 more pool remains after the second page");

    // Page 3: the last row; <= limit rows returned, so next_cursor is unset.
    let (page3, cursor3) = svc.list_page(Some(2), Some(cursor2)).await.expect("page3");
    assert_eq!(
        page3.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
        expected_order[4..5]
    );
    assert!(cursor3.is_none(), "no rows remain after the last page");
}

#[tokio::test]
async fn rate_limit_create_rejected_when_disabled() {
    // Creating a policy on the disabled backend is rejected (403 /
    // FAILED_PRECONDITION) and the list stays empty. CRUD and pagination
    // against the *enabled* backends live in `ratelimit_conformance_test.rs`.
    let svc = rate_limit_service();
    let err = svc
        .create(CreateRateLimitInput {
            limit_type: "requests_per_minute".into(),
            limit_value: 10,
            burst_value: None,
            connection_id: Some("conn-1".into()),
            models: vec!["gpt-4o".into()],
            scope: "usage_pool".into(),
            scope_id: "pool-1".into(),
            enabled: true,
        })
        .await
        .unwrap_err();

    assert!(matches!(err, ServiceError::FailedPrecondition { .. }));
    assert_eq!(err.http_status().as_u16(), 403);
    assert_eq!(svc.list().await.len(), 0);
}

#[tokio::test]
async fn rate_limit_create_disabled_policy_allowed_when_backend_disabled() {
    // `create` mirrors `update`'s gate: a disabled policy takes no effect,
    // so authoring one is allowed even while quota enforcement is off.
    let svc = rate_limit_service();
    let created = svc
        .create(CreateRateLimitInput {
            limit_type: "requests_per_minute".into(),
            limit_value: 10,
            burst_value: None,
            connection_id: Some("conn-1".into()),
            models: vec!["gpt-4o".into()],
            scope: "usage_pool".into(),
            scope_id: "pool-1".into(),
            enabled: false,
        })
        .await
        .expect("a disabled policy may be authored while quota is off");
    assert!(!created.enabled);

    // Flipping it on while still disabled is rejected, same as `update`.
    let err = svc
        .update(
            &created.id,
            UpdateRateLimitInput {
                enabled: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ServiceError::FailedPrecondition { .. }));
}

#[tokio::test]
async fn rate_limit_environment_scope_is_rejected() {
    let svc = RateLimitService::new(
        Arc::new(MemoryRateLimitPolicyStore::new("test".to_string())),
        true,
    );
    let error = svc
        .create(CreateRateLimitInput {
            limit_type: "requests_per_minute".into(),
            limit_value: 10,
            burst_value: None,
            connection_id: Some("conn-1".into()),
            models: Vec::new(),
            scope: "environment".into(),
            scope_id: String::new(),
            enabled: true,
        })
        .await
        .expect_err("environment-wide quota scope is not supported");
    assert!(matches!(error, ServiceError::InvalidArgument { .. }));
}

#[tokio::test]
async fn global_rate_limit_requires_no_model_selector() {
    let svc = RateLimitService::new(
        Arc::new(MemoryRateLimitPolicyStore::new("test".to_string())),
        true,
    );
    let global = svc
        .create(CreateRateLimitInput {
            limit_type: "requests_per_minute".into(),
            limit_value: 10,
            burst_value: None,
            connection_id: None,
            models: Vec::new(),
            scope: "api_key".into(),
            scope_id: "key-1".into(),
            enabled: true,
        })
        .await
        .expect("global rate limit is valid");
    assert!(global.connection_id.is_none());

    let error = svc
        .create(CreateRateLimitInput {
            limit_type: "requests_per_minute".into(),
            limit_value: 10,
            burst_value: None,
            connection_id: None,
            models: vec!["gpt-4o".into()],
            scope: "api_key".into(),
            scope_id: "key-1".into(),
            enabled: true,
        })
        .await
        .expect_err("models without a connection are ambiguous");
    assert!(matches!(error, ServiceError::InvalidArgument { .. }));
}

#[tokio::test]
async fn rate_limit_create_rejects_wildcard_models_without_storing_policies() {
    let svc = RateLimitService::new(
        Arc::new(MemoryRateLimitPolicyStore::new("test".to_string())),
        true,
    );
    for scope in ["api_key", "usage_pool"] {
        for model in ["*", "openai/*", "bedrock/openai.*"] {
            let error = svc
                .create(CreateRateLimitInput {
                    limit_type: "requests_per_minute".into(),
                    limit_value: 1,
                    burst_value: None,
                    connection_id: Some("openai".into()),
                    models: vec![model.into()],
                    scope: scope.into(),
                    scope_id: "fixture".into(),
                    enabled: true,
                })
                .await
                .expect_err("wildcard selectors would never enforce the limit");
            assert!(matches!(
                error,
                ServiceError::InvalidArgument { field: Some(ref field), ref message }
                    if field == "models" && message.contains("wildcard")
                        && message.contains("exact public model ids")
            ));
        }
    }
    assert!(svc.list().await.is_empty());
}

#[tokio::test]
async fn rate_limit_update_rejects_wildcards_and_preserves_existing_policy() {
    let svc = RateLimitService::new(
        Arc::new(MemoryRateLimitPolicyStore::new("test".to_string())),
        true,
    );
    for scope in ["api_key", "usage_pool"] {
        let created = svc
            .create(CreateRateLimitInput {
                limit_type: "requests_per_minute".into(),
                limit_value: 1,
                burst_value: None,
                connection_id: Some("openai".into()),
                models: vec!["openai/gpt-6-luna".into()],
                scope: scope.into(),
                scope_id: "fixture".into(),
                enabled: true,
            })
            .await
            .expect("exact public model ids are valid");
        let error = svc
            .update(
                &created.id,
                UpdateRateLimitInput {
                    models: Some(vec![" openai/* ".into()]),
                    limit_value: Some(99),
                    enabled: Some(false),
                    ..Default::default()
                },
            )
            .await
            .expect_err("even disabled policies cannot store wildcard selectors");
        assert!(matches!(
            error,
            ServiceError::InvalidArgument { field: Some(ref field), .. } if field == "models"
        ));
        let stored = svc.get(&created.id).await.expect("original policy remains");
        assert_eq!(
            serde_json::to_value(&stored).unwrap(),
            serde_json::to_value(&created).unwrap(),
        );
    }
}

#[tokio::test]
async fn rate_limits_accept_exact_ids_and_connection_wide_empty_models() {
    let svc = RateLimitService::new(
        Arc::new(MemoryRateLimitPolicyStore::new("test".to_string())),
        true,
    );
    for scope in ["api_key", "usage_pool"] {
        for models in [vec![" openai/gpt-6-luna ".into()], Vec::new()] {
            let created = svc
                .create(CreateRateLimitInput {
                    limit_type: "requests_per_minute".into(),
                    limit_value: 1,
                    burst_value: None,
                    connection_id: Some("openai".into()),
                    models,
                    scope: scope.into(),
                    scope_id: "fixture".into(),
                    enabled: true,
                })
                .await
                .expect("exact and connection-wide selectors are valid");
            assert!(created.models.is_empty() || created.models == ["openai/gpt-6-luna"]);
            svc.delete(&created.id).await.expect("remove policy");
        }
        let created = svc
            .create(CreateRateLimitInput {
                limit_type: "requests_per_minute".into(),
                limit_value: 1,
                burst_value: None,
                connection_id: Some("openai".into()),
                models: vec!["openai/gpt-6-luna".into()],
                scope: scope.into(),
                scope_id: "fixture".into(),
                enabled: true,
            })
            .await
            .expect("create exact selector");
        let updated = svc
            .update(
                &created.id,
                UpdateRateLimitInput {
                    models: Some(Vec::new()),
                    ..Default::default()
                },
            )
            .await
            .expect("empty models cover the entire connection");
        assert_eq!(updated.connection_id.as_deref(), Some("openai"));
        assert!(updated.models.is_empty());
        let updated = svc
            .update(
                &created.id,
                UpdateRateLimitInput {
                    models: Some(vec![" openai/gpt-6-luna ".into()]),
                    ..Default::default()
                },
            )
            .await
            .expect("exact ids remain valid on update");
        assert_eq!(updated.models, ["openai/gpt-6-luna"]);
    }
}

#[tokio::test]
async fn rate_limit_update_revalidates_and_preserves_policy_identity_and_ownership() {
    let svc = RateLimitService::new(
        Arc::new(MemoryRateLimitPolicyStore::new("test".to_string())),
        true,
    );
    let created = svc
        .create(CreateRateLimitInput {
            limit_type: "requests_per_minute".into(),
            limit_value: 10,
            burst_value: None,
            connection_id: Some("conn-1".into()),
            models: vec!["model-a".into()],
            scope: "usage_pool".into(),
            scope_id: "pool-1".into(),
            enabled: true,
        })
        .await
        .expect("create rate limit");

    let updated = svc
        .update(
            &created.id,
            UpdateRateLimitInput {
                limit_type: Some("tokens_per_minute".into()),
                limit_value: Some(100),
                burst_value: Some(Some(150)),
                connection_id: Some(Some(" conn-2 ".into())),
                models: Some(vec!["model-b".into(), "model-b".into()]),
                ..Default::default()
            },
        )
        .await
        .expect("update rate limit");

    assert_eq!(updated.id, created.id);
    assert_eq!(updated.quota_policy_id, created.quota_policy_id);
    assert_eq!(updated.created_at, created.created_at);
    assert_eq!(updated.limit_type, "tokens_per_minute");
    assert_eq!(updated.limit_value, 100);
    assert_eq!(updated.burst_value, Some(150));
    assert_eq!(updated.connection_id.as_deref(), Some("conn-2"));
    assert_eq!(updated.models, ["model-b"]);
    assert_eq!(updated.scope, "usage_pool");
    assert_eq!(updated.scope_id, "pool-1");

    let error = svc
        .update(
            &created.id,
            UpdateRateLimitInput {
                connection_id: Some(None),
                ..Default::default()
            },
        )
        .await
        .expect_err("model selectors require a connection");
    assert!(matches!(error, ServiceError::InvalidArgument { .. }));
}

#[tokio::test]
async fn rate_limit_get_returns_created_policy() {
    let svc = RateLimitService::new(
        Arc::new(MemoryRateLimitPolicyStore::new("test".to_string())),
        true,
    );
    let created = svc
        .create(CreateRateLimitInput {
            limit_type: "requests_per_minute".into(),
            limit_value: 10,
            burst_value: None,
            connection_id: None,
            models: Vec::new(),
            scope: "usage_pool".into(),
            scope_id: "pool-1".into(),
            enabled: true,
        })
        .await
        .expect("create rate limit");

    let fetched = svc.get(&created.id).await.expect("get");
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.limit_value, 10);
}

#[tokio::test]
async fn rate_limit_get_unknown_is_not_found() {
    let svc = rate_limit_service();
    let err = svc.get("nope").await.unwrap_err();
    assert!(matches!(err, ServiceError::NotFound { .. }));
}

#[tokio::test]
async fn rate_limit_get_rejects_empty_id() {
    let svc = rate_limit_service();
    let err = svc.get("   ").await.unwrap_err();
    assert!(matches!(err, ServiceError::InvalidArgument { .. }));
}

#[tokio::test]
async fn usage_budget_get_returns_created_budget() {
    let (pool_service, budget_service) = usage_pool_and_budget_services();
    let pool = pool_service
        .create(CreateUsagePoolInput {
            name: "pool".into(),
            ..Default::default()
        })
        .await
        .expect("create pool");

    let created = budget_service
        .create(
            "usage_pool".into(),
            pool.id.clone(),
            100,
            "daily".into(),
            true,
        )
        .await
        .expect("create budget");

    let fetched = budget_service.get(&created.id).await.expect("get");
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.limit_amount, 100);
}

#[tokio::test]
async fn usage_budget_get_unknown_is_not_found() {
    let (_pool_service, budget_service) = usage_pool_and_budget_services();
    let err = budget_service.get("nope").await.unwrap_err();
    assert!(matches!(err, ServiceError::NotFound { .. }));
}

#[tokio::test]
async fn fallback_policy_round_trip() {
    let svc = fallback_service();
    assert!(svc.get().await.expect("get").is_empty());

    let mut map = std::collections::HashMap::new();
    map.insert("gpt-4o".to_string(), vec!["gpt-4o-mini".to_string()]);
    svc.set(map.clone())
        .await
        .expect("in-memory fallback set never fails");

    assert_eq!(svc.get().await.expect("get"), map);
}

#[tokio::test]
async fn service_error_maps_to_both_http_and_grpc() {
    let err = ServiceError::invalid_field("name", "must not be empty");
    let (status, _body) = err.clone().into_http();
    assert_eq!(status.as_u16(), 400);

    let grpc: tonic::Status = err.into();
    assert_eq!(grpc.code(), tonic::Code::InvalidArgument);
    assert!(grpc.message().contains("must not be empty"));
}

/// The standard test policy: enabled, judge on `openai/judge`, one ladder for
/// `openai/big` -> [`openai/mid`, `openai/cheap`].
fn base_policy() -> JudgePolicy {
    let mut ladders = HashMap::new();
    ladders.insert(
        "openai/big".to_string(),
        JudgeLadder {
            targets: vec!["openai/mid".to_string(), "openai/cheap".to_string()],
            boundaries: vec![],
        },
    );
    JudgePolicy {
        revision: 0,
        enabled: true,
        shadow: false,
        shadow_sample_rate: 0.0,
        judge_model: "openai/judge".to_string(),
        ladders,
        default_opt_in: false,
        default_floor: None,
    }
}

#[tokio::test]
async fn policy_update_rejects_malformed_boundaries() {
    let store: Arc<dyn openai_server::judge_policy::JudgePolicyStore> =
        Arc::new(InMemoryJudgePolicyStore::new(JudgePolicy::default()));
    let svc = openai_server::service::JudgePolicyService::new(store.clone());

    let mut policy = base_policy();
    policy.ladders.get_mut("openai/big").unwrap().boundaries = vec![0.5];
    assert!(
        svc.set(policy).await.is_err(),
        "1 boundary on a 3-rung ladder must be rejected at write time"
    );
    assert!(
        !store.get().await.unwrap().enabled,
        "rejected policy must not persist"
    );

    // The same ladder with a correctly-sized list persists.
    let mut policy = base_policy();
    policy.ladders.get_mut("openai/big").unwrap().boundaries = vec![0.3, 0.7];
    assert!(svc.set(policy).await.is_ok());
    assert!(store.get().await.unwrap().enabled);
}

#[tokio::test]
async fn set_cas_conflict_maps_to_failed_precondition_not_internal() {
    // A stale `expected` (e.g. a second admin session that read the policy
    // before another edit landed) must surface as a client-actionable
    // FAILED_PRECONDITION, never a generic 500 — the conflict is exactly the
    // information a retry-after-re-read UI flow needs.
    let store: Arc<dyn openai_server::judge_policy::JudgePolicyStore> =
        Arc::new(InMemoryJudgePolicyStore::new(JudgePolicy::default()));
    let svc = openai_server::service::JudgePolicyService::new(store.clone());

    let stale_expected = store.get().await.unwrap();
    // Someone else's concurrent write moves the store past the stale view.
    svc.set(base_policy()).await.expect("first write succeeds");

    let err = svc
        .set_cas(&stale_expected, base_policy())
        .await
        .expect_err("stale expected revision must conflict");
    assert!(
        matches!(
            err,
            openai_server::service::ServiceError::FailedPrecondition { .. }
        ),
        "a CAS conflict must map to FailedPrecondition, not Internal; got {err:?}"
    );
}

#[tokio::test]
async fn usage_pool_rate_limits_reject_wildcards_with_nested_field_path() {
    let svc = UsagePoolService::new(
        Arc::new(InMemoryUsagePoolStore::new()),
        Arc::new(MemoryBudgetPolicyStore::new("test".to_string())),
        Arc::new(MemoryRateLimitPolicyStore::new("test".to_string())),
        "test".to_string(),
        true,
    );
    let error = svc
        .create(CreateUsagePoolInput {
            name: "wildcard limit".into(),
            usage_budgets: Vec::new(),
            rate_limits: vec![ApiKeyRateLimitInput {
                connection_id: Some("openai".into()),
                models: vec!["openai/*".into()],
                requests_per_minute: Some(1),
                request_burst: None,
                tokens_per_minute: None,
                token_burst: None,
                concurrent_requests: None,
                enabled: true,
            }],
        })
        .await
        .expect_err("pool selectors follow the same exact-id rule");
    assert!(matches!(
        error,
        ServiceError::InvalidArgument { field: Some(ref field), .. }
            if field == "rate_limits[0].models"
    ));
    assert!(svc.list().await.is_empty());
}

#[tokio::test]
async fn api_key_model_allowlists_reject_wildcards() {
    let store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
        b"test-pepper".to_vec(),
    )));
    let svc = ApiKeyService::new(
        store,
        None,
        Arc::new(MemoryBudgetPolicyStore::new("test".to_string())),
        Arc::new(MemoryRateLimitPolicyStore::new("test".to_string())),
        std::sync::Arc::new(
            openai_server::provider_connections::InMemoryProviderConnectionStore::new(),
        ),
        std::sync::Arc::new(openai_server::model_routes::InMemoryModelRouteStore::new()),
        true,
    );
    let scoped = |model: &str| ApiKeyPermission {
        connection_scopes: Some(vec![ApiKeyConnectionScope {
            connection_id: "conn".to_string(),
            models: vec![model.to_string()],
        }]),
        ..Default::default()
    };
    for permission in [scoped("openai/*"), scoped("bedrock/openai.*")] {
        let error = svc
            .create(CreateKeyInput {
                description: Some("wildcard".to_string()),
                permission: Some(permission.into()),
                ..Default::default()
            })
            .await
            .expect_err("a wildcard would be stored but match nothing");
        assert!(matches!(error, ServiceError::InvalidArgument { .. }));
    }

    // Rate-limit model lists match exactly too.
    let error = svc
        .create(CreateKeyInput {
            description: Some("wildcard limit".to_string()),
            rate_limits: vec![ApiKeyRateLimitInput {
                connection_id: Some("conn".to_string()),
                models: vec!["openai/*".to_string()],
                requests_per_minute: Some(60),
                request_burst: None,
                tokens_per_minute: None,
                token_burst: None,
                concurrent_requests: None,
                enabled: true,
            }],
            ..Default::default()
        })
        .await
        .expect_err("a wildcard limit would never apply");
    assert!(
        matches!(error, ServiceError::InvalidArgument { .. }),
        "{error:?}"
    );

    let created = svc
        .create(CreateKeyInput {
            description: Some("exact".to_string()),
            permission: Some(scoped("openai/gpt-6-luna").into()),
            ..Default::default()
        })
        .await
        .expect("exact model ids are accepted");
    let error = svc
        .update(
            &created.api_key.key_hash,
            UpdateKeyInput {
                permission: Some(scoped("openai/*").into()),
                ..Default::default()
            },
        )
        .await
        .expect_err("updates are validated like creates");
    assert!(matches!(error, ServiceError::InvalidArgument { .. }));
}
