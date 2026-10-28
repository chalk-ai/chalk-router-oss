//! Store-level isolation of two environments that share one Postgres pool.
//!
//! A cluster router gives every environment its own stores, but all of them run
//! the same SQL against the same tables, and isolation is nothing more than
//! `WHERE environment_id = $n`. This test acts as environment B against rows
//! that belong to environment A, with A's real identifiers, through every
//! store a runtime owns. B must see nothing, change nothing, and leave A's rows
//! exactly as A wrote them. In-memory runtimes cannot share rows, so this is
//! Postgres-only.

use std::{collections::HashMap, sync::Arc};

use chrono::Utc;
use openai_server::{
    api_keys::{ApiKeyPermission, PostgresApiKeyStore},
    budget::ApiKeyBudgetInput,
    config::{QuotaBackendKind, RouterConfig, UsageBackendKind},
    environment_runtime::RuntimeFactory,
    hashing::KeyHasher,
    model_routes::{RouteInput, RouteStoreError, RouteUpdate},
    provider_connections::{
        ConnectionInput, ConnectionUpdate, DeleteConnectionOutcome, ExposurePolicy, StoreError,
    },
    revisioned_policy::RevisionedPolicy,
    sampling_policy::SamplingPolicy,
    secrets::SecretCipher,
    telemetry_settings::{
        PostgresTelemetryExporterStore, TelemetryExporter, TelemetrySettingsStore,
    },
    usage_tracker::{UsageRecord, UsageSource},
    AppState,
};
use serde_json::json;

use crate::support::app::AppStateBuilder;

/// Two environments, built the way a cluster builds them: one pool, one KEK, one pepper.
struct Tenants {
    a: AppState,
    b: AppState,
    pool: sqlx::PgPool,
    hasher: KeyHasher,
}

async fn tenants(pool: sqlx::PgPool) -> Tenants {
    let a_id = format!("iso-a-{}", uuid::Uuid::new_v4());
    let b_id = format!("iso-b-{}", uuid::Uuid::new_v4());
    let mut config = RouterConfig::from_env();
    config.usage_backend = UsageBackendKind::Postgres;
    config.quota_backend = QuotaBackendKind::Memory;
    let hasher = KeyHasher::new(uuid::Uuid::new_v4().as_bytes().to_vec());
    let factory = RuntimeFactory {
        shared: AppStateBuilder::new(Arc::new(openai_server::backend::RouterBackend::new(
            reqwest::Client::new(),
            Arc::new(openai_server::provider_connections::InMemoryProviderConnectionStore::new()),
            Arc::new(openai_server::model_routes::InMemoryModelRouteStore::new()),
        )))
        .build()
        .await,
        pool: Some(pool.clone()),
        cipher: Some(
            SecretCipher::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap(),
        ),
        config,
        hasher: hasher.clone(),
    };
    let (a, _) = factory.build(&a_id).await.unwrap();
    let (b, _) = factory.build(&b_id).await.unwrap();
    Tenants { a, b, pool, hasher }
}

fn connection(name: &str, credential: &str) -> ConnectionInput {
    ConnectionInput {
        name: name.into(),
        provider_kind: "openai".into(),
        base_url: None,
        prefix: Some(format!("{name}/")),
        exposure: ExposurePolicy::Dynamic,
        routing_enabled: true,
        declared_capabilities: None,
        api_key: credential.into(),
    }
}

#[tokio::test]
async fn postgres_environments_sharing_a_pool_cannot_read_or_change_each_other() {
    let Some(url) = crate::support::db::test_database_url("environment isolation") else {
        return;
    };
    let _lock = crate::support::db::connect_locked(&url).await;
    openai_server::migrate::run(&url).await.unwrap();
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let t = tenants(pool.clone()).await;

    api_keys_are_private(&t).await;
    usage_pools_are_private(&t).await;
    provider_connections_and_routes_are_private(&t).await;
    policies_and_exporters_are_private(&t).await;
    budgets_and_rate_limits_are_private(&t).await;
    usage_is_private(&t).await;

    for state in [&t.a, &t.b] {
        if let Some(tracker) = &state.usage_tracker {
            tracker.shutdown().await;
        }
    }
    pool.close().await;
}

async fn api_keys_are_private(t: &Tenants) {
    let key =
        t.a.api_key_store
            .create(
                "a's key".into(),
                ApiKeyPermission::default(),
                HashMap::new(),
                None,
            )
            .await
            .unwrap();
    let hash = t.hasher.hash(&key.raw_key);

    assert!(t.a.api_key_store.get(&hash).await.is_some());
    // B can neither find, list, authenticate, nor rewrite A's key.
    assert!(t.b.api_key_store.get(&hash).await.is_none());
    assert!(t.b.api_key_store.list().await.is_empty());
    assert!(t.b.api_key_store.authenticate(&key.raw_key).await.is_none());
    let updated =
        t.b.api_key_store
            .update(&hash, Some("hijacked".into()), None, None)
            .await
            .unwrap();
    assert!(updated.is_none(), "B updated A's key");
    assert_eq!(
        t.a.api_key_store.get(&hash).await.unwrap().description,
        "a's key"
    );
    // The key directory is the one deliberate cross-environment read: it must
    // name A, and only A, as the owner.
    let (owner, _) = PostgresApiKeyStore::authenticate_cluster(&t.pool, &t.hasher, &key.raw_key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owner, t.a.environment_id);
    // B's own key with the same description is its own row.
    let b_key =
        t.b.api_key_store
            .create(
                "a's key".into(),
                ApiKeyPermission::default(),
                HashMap::new(),
                None,
            )
            .await
            .unwrap();
    assert!(t
        .a
        .api_key_store
        .authenticate(&b_key.raw_key)
        .await
        .is_none());
    assert_eq!(t.a.api_key_store.list().await.len(), 1);
    assert_eq!(t.b.api_key_store.list().await.len(), 1);
}

async fn usage_pools_are_private(t: &Tenants) {
    let pool =
        t.a.usage_pool_store
            .create_pool("shared".into())
            .await
            .unwrap();
    assert!(t
        .b
        .usage_pool_store
        .get_pool(&pool.id)
        .await
        .unwrap()
        .is_none());
    assert!(t.b.usage_pool_store.list_pools().await.is_empty());
    assert!(t
        .b
        .usage_pool_store
        .update_pool(&pool.id, "hijacked".into())
        .await
        .unwrap()
        .is_none());
    assert!(!t.b.usage_pool_store.delete_pool(&pool.id).await.unwrap());
    assert_eq!(
        t.a.usage_pool_store
            .get_pool(&pool.id)
            .await
            .unwrap()
            .unwrap()
            .name,
        "shared"
    );
}

async fn provider_connections_and_routes_are_private(t: &Tenants) {
    let a_connection =
        t.a.provider_connection_store
            .create(connection("shared-name", "a-secret"))
            .await
            .unwrap();
    // The same name and prefix are free in another environment.
    let b_connection =
        t.b.provider_connection_store
            .create(connection("shared-name", "b-secret"))
            .await
            .unwrap();

    assert!(t
        .b
        .provider_connection_store
        .get(&a_connection.id)
        .await
        .unwrap()
        .is_none());
    assert!(t
        .b
        .provider_connection_store
        .get_metadata(&a_connection.id)
        .await
        .unwrap()
        .is_none());
    assert!(matches!(
        t.b.provider_connection_store
            .update(
                &a_connection.id,
                ConnectionUpdate {
                    api_key: Some("hijacked".into()),
                    ..Default::default()
                },
            )
            .await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        t.b.provider_connection_store
            .delete_if_unreferenced(&a_connection.id)
            .await,
        Ok(DeleteConnectionOutcome::NotFound)
    ));
    let a_stored =
        t.a.provider_connection_store
            .get(&a_connection.id)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        a_stored.metadata().credential_state,
        openai_server::provider_connections::CredentialState::Present
    );
    assert_eq!(listed_ids(&t.b).await, vec![b_connection.id.clone()]);

    // Model routes.
    let route =
        t.a.model_route_store
            .create(RouteInput {
                public_model_id: "chalk/shared".into(),
                connection_id: a_connection.id.clone(),
                upstream_model_id: "gpt-4o".into(),
                published: true,
                routing: None,
            })
            .await
            .unwrap();
    // Grant resolution must use the request's environment even when the
    // caller knows another environment's UUID or public alias.
    use openai_server::service::{
        api_keys::{ApiKeyPermissionInput, ApiKeyRouteRef, CreateKeyInput},
        ServiceError,
    };
    for identifier in [
        ApiKeyRouteRef::RouteId(route.id.clone()),
        ApiKeyRouteRef::PublicModelId(route.public_model_id.clone()),
    ] {
        let mut permission = ApiKeyPermissionInput::default();
        permission.routes = Some(vec![identifier]);
        let error =
            t.b.api_key_service()
                .create(CreateKeyInput {
                    description: Some("foreign route".into()),
                    permission: Some(permission),
                    ..Default::default()
                })
                .await
                .unwrap_err();
        assert!(matches!(error, ServiceError::InvalidArgument { .. }));
    }
    assert!(t
        .b
        .model_route_store
        .get_by_public_id("chalk/shared")
        .await
        .unwrap()
        .is_none());
    assert!(t.b.model_route_store.list().await.unwrap().is_empty());
    assert!(t
        .b
        .model_route_store
        .list_by_connection(&a_connection.id)
        .await
        .unwrap()
        .is_empty());
    assert!(matches!(
        t.b.model_route_store
            .update(
                &route.id,
                RouteUpdate {
                    upstream_model_id: Some("hijacked".into()),
                    ..Default::default()
                },
            )
            .await,
        Err(RouteStoreError::NotFound)
    ));
    assert_eq!(t.b.model_route_store.delete(&route.id).await.unwrap(), None);
    // A route's connection is a soft reference by design, so B can write one
    // that names A's connection id. It must stay inert: B's routing sees only
    // B's connections, so it never reaches A's credential, and it does not
    // count against A's connection.
    t.b.model_route_store
        .create(RouteInput {
            public_model_id: "chalk/stolen".into(),
            connection_id: a_connection.id.clone(),
            upstream_model_id: "gpt-4o".into(),
            published: true,
            routing: None,
        })
        .await
        .unwrap();
    t.b.backend.invalidate_routing_cache().await;
    assert!(
        t.b.backend.resolve("chalk/stolen").await.is_err(),
        "B resolved a route through A's connection"
    );
    assert_eq!(
        t.a.model_route_store
            .list_by_connection(&a_connection.id)
            .await
            .unwrap()
            .len(),
        1,
        "B's route shows up against A's connection"
    );
    let spare =
        t.a.provider_connection_store
            .create(connection("spare", "a-spare-secret"))
            .await
            .unwrap();
    t.b.model_route_store
        .create(RouteInput {
            public_model_id: "chalk/spare".into(),
            connection_id: spare.id.clone(),
            upstream_model_id: "gpt-4o".into(),
            published: true,
            routing: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(
            t.a.provider_connection_store
                .delete_if_unreferenced(&spare.id)
                .await,
            Ok(DeleteConnectionOutcome::Deleted { .. })
        ),
        "B's route blocked A from deleting its own connection"
    );
    // The same alias is free in B, and points at B's connection.
    let b_route =
        t.b.model_route_store
            .create(RouteInput {
                public_model_id: "chalk/shared".into(),
                connection_id: b_connection.id.clone(),
                upstream_model_id: "gpt-4o".into(),
                published: true,
                routing: None,
            })
            .await
            .unwrap();
    assert_ne!(b_route.id, route.id);
    // The same name binds to a different UUID in each environment; persisted
    // permissions contain no name that could be rebound on a later request.
    for (state, own_route) in [(&t.a, &route), (&t.b, &b_route)] {
        let mut permission = ApiKeyPermissionInput::default();
        permission.routes = Some(vec![ApiKeyRouteRef::PublicModelId("chalk/shared".into())]);
        let created = state
            .api_key_service()
            .create(CreateKeyInput {
                description: Some("own route".into()),
                permission: Some(permission),
                ..Default::default()
            })
            .await
            .unwrap();
        let stored = state
            .api_key_store
            .get(&created.api_key.key_hash)
            .await
            .unwrap();
        assert_eq!(
            stored.permission.route_ids,
            Some(vec![own_route.id.clone()])
        );
        let stored_json: serde_json::Value = sqlx::query_scalar(
            "SELECT permission FROM chalk_router.api_keys WHERE environment_id = $1 AND key_hash = $2"
        ).bind(&state.environment_id).bind(&stored.key_hash).fetch_one(&t.pool).await.unwrap();
        assert_eq!(stored_json["route_ids"], json!([own_route.id]));
        assert!(!stored_json.to_string().contains("chalk/shared"));
    }

    assert_eq!(
        t.a.model_route_store
            .get_by_public_id("chalk/shared")
            .await
            .unwrap()
            .unwrap()
            .connection_id,
        a_connection.id
    );
    assert_eq!(
        t.a.model_route_store.list().await.unwrap().len(),
        1,
        "A's routes"
    );
}

async fn listed_ids(state: &AppState) -> Vec<String> {
    state
        .provider_connection_store
        .list()
        .await
        .unwrap()
        .iter()
        .map(|connection| connection.id().to_string())
        .collect()
}

async fn policies_and_exporters_are_private(t: &Tenants) {
    // Sampling policy: a singleton row per environment.
    let mut policy = SamplingPolicy::default();
    policy.enabled = true;
    policy.default_rate = 0.25;
    t.a.sampling_policy.set(policy).await.unwrap();
    let b_policy = t.b.sampling_policy.get().await.unwrap();
    assert!(!b_policy.enabled, "B inherited A's sampling policy");
    assert_eq!(b_policy.revision(), 0);
    assert_eq!(t.a.sampling_policy.get().await.unwrap().default_rate, 0.25);
    // A compare-and-set against A's revision must not move B's policy.
    let a_policy = t.a.sampling_policy.get().await.unwrap();
    let mut next = b_policy.clone();
    next.default_rate = 0.9;
    let _ = t.b.sampling_policy.set_cas(&a_policy, next).await;
    assert_eq!(t.a.sampling_policy.get().await.unwrap().default_rate, 0.25);

    // Telemetry exporters hold encrypted destination credentials.
    let exporters = |environment: &str| {
        PostgresTelemetryExporterStore::new(
            t.pool.clone(),
            environment.to_string(),
            SecretCipher::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap(),
            Vec::new(),
        )
    };
    let (a_exporters, b_exporters) = (
        exporters(&t.a.environment_id),
        exporters(&t.b.environment_id),
    );
    let now = Utc::now();
    a_exporters
        .upsert(
            TelemetryExporter {
                id: "exporter-a".into(),
                name: "a".into(),
                config: json!({"type": "braintrust", "project": "a"}),
                enabled: true,
                read_only: false,
                api_key: String::new(),
                created_at: now,
                updated_at: now,
            },
            Some("a-braintrust-secret".into()),
        )
        .await
        .unwrap();
    assert!(b_exporters.list().await.unwrap().is_empty());
    assert!(!b_exporters.delete("exporter-a").await.unwrap());
    let kept = a_exporters.list().await.unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].api_key, "a-braintrust-secret");
}

async fn budgets_and_rate_limits_are_private(t: &Tenants) {
    use openai_server::ratelimit::{RateLimitMatch, RateLimitModel, RateLimitPolicy};

    let key =
        t.a.api_key_store
            .create(
                "limited".into(),
                ApiKeyPermission::default(),
                HashMap::new(),
                None,
            )
            .await
            .unwrap();
    let hash = t.hasher.hash(&key.raw_key);

    t.a.budget_store
        .replace_key_budgets(
            &hash,
            vec![ApiKeyBudgetInput {
                token_limit: 1_000,
                period: "daily".into(),
                enabled: true,
            }],
        )
        .await
        .unwrap();
    assert!(t
        .a
        .budget_store
        .get_key_daily_budget(&hash)
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        t.b.budget_store.get_key_daily_budget(&hash).await.unwrap(),
        None
    );
    assert!(t
        .b
        .budget_store
        .matching(&hash, None)
        .await
        .unwrap()
        .is_empty());
    assert!(t.b.budget_store.list().await.unwrap().is_empty());
    let a_budget = t.a.budget_store.list().await.unwrap().remove(0);
    assert!(t.b.budget_store.get(&a_budget.id).await.unwrap().is_none());
    assert!(t
        .b
        .budget_store
        .update(&a_budget.id, Some(1), Some(false))
        .await
        .is_err());
    let _ = t.b.budget_store.delete(&a_budget.id).await;
    let after = t.a.budget_store.get(&a_budget.id).await.unwrap().unwrap();
    assert_eq!((after.limit_amount, after.enabled), (1_000, true));

    let policy =
        t.a.rate_limit_store
            .create(RateLimitPolicy {
                quota_policy_id: format!("quota-{}-limit", t.a.environment_id),
                id: format!("limit-{}", t.a.environment_id),
                limit_type: "requests_per_minute".into(),
                limit_value: 10,
                burst_value: None,
                connection_id: None,
                models: Vec::new(),
                scope: "api_key".into(),
                scope_id: hash.clone(),
                enabled: true,
                created_at: Utc::now(),
            })
            .await
            .unwrap();
    assert!(t.b.rate_limit_store.get(&policy.id).await.is_err());
    assert!(t.b.rate_limit_store.list().await.is_empty());
    let request = RateLimitMatch {
        key_hash: &hash,
        usage_pool_id: None,
        connection_id: None,
        model: RateLimitModel::NotApplicable,
    };
    assert!(t
        .b
        .rate_limit_store
        .matching(&request)
        .await
        .unwrap()
        .is_empty());
    let mut changed = policy.clone();
    changed.limit_value = 1;
    assert!(t.b.rate_limit_store.update(changed).await.is_err());
    let _ = t.b.rate_limit_store.delete(&policy.id).await;
    let _ = t.b.rate_limit_store.delete_for_api_key(&hash).await;
    let after = t.a.rate_limit_store.get(&policy.id).await.unwrap();
    assert_eq!(after.limit_value, 10);
    assert_eq!(
        t.a.rate_limit_store.matching(&request).await.unwrap().len(),
        1
    );
}

async fn usage_is_private(t: &Tenants) {
    let a = t.a.usage_tracker.as_ref().expect("usage tracker");
    let b = t.b.usage_tracker.as_ref().expect("usage tracker");
    let key = t.hasher.hash("usage-key");
    let event = uuid::Uuid::new_v4();
    // Identical identity written by both: events are unique per environment.
    for tracker in [a, b] {
        tracker
            .record(UsageRecord {
                event_id: event,
                time: Utc::now(),
                key_hash: &key,
                usage_pool_id: None,
                connection_id: "connection",
                provider_kind: "openai",
                model: "gpt-4o",
                prompt_tokens: 3,
                completion_tokens: 2,
                cached_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
                usage_source: UsageSource::Reported,
                input_token_attribution: Default::default(),
            })
            .await
            .unwrap();
    }
    for state in [&t.a, &t.b] {
        let tracker = state.usage_tracker.as_ref().unwrap();
        let snapshots = tracker.snapshots(std::slice::from_ref(&key)).await.unwrap();
        assert_eq!(snapshots[&key].total_tokens, 5, "{}", state.environment_id);
        assert_eq!(snapshots[&key].request_count, 1);
    }
    // A key that only A's ledger knows has no usage in B.
    let a_only = t.hasher.hash("a-only-usage-key");
    a.record(UsageRecord {
        event_id: uuid::Uuid::new_v4(),
        time: Utc::now(),
        key_hash: &a_only,
        usage_pool_id: None,
        connection_id: "connection",
        provider_kind: "openai",
        model: "gpt-4o",
        prompt_tokens: 1,
        completion_tokens: 1,
        cached_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        usage_source: UsageSource::Reported,
        input_token_attribution: Default::default(),
    })
    .await
    .unwrap();
    a.shutdown().await;
    b.shutdown().await;
    let seen_by_b = b.snapshots(std::slice::from_ref(&a_only)).await.unwrap();
    assert!(seen_by_b.get(&a_only).is_none_or(|s| s.total_tokens == 0));
    let seen_by_a = a.snapshots(std::slice::from_ref(&a_only)).await.unwrap();
    assert_eq!(seen_by_a[&a_only].total_tokens, 2);
}
