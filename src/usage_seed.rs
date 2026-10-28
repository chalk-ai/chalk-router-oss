//! `openai-server seed-usage`: add an idempotent, local-only usage fixture.
//!
//! The command is deliberately additive. It creates a small set of clearly
//! named, disabled demo resources and refreshes only stable demo event
//! identities, so repeated runs never duplicate the chart data.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{Duration, Utc};

use crate::api_keys::{ApiKey, ApiKeyPermission, ApiKeyStore, PostgresApiKeyStore};
use crate::config::RouterConfig;
use crate::hashing::KeyHasher;
use crate::model_routes::{ModelRoute, ModelRouteStore, PostgresModelRouteStore, RouteInput};
use crate::provider_connections::{
    ConnectionInput, ExposurePolicy, PostgresProviderConnectionStore, StoredConnection,
    KEYLESS_PLACEHOLDER,
};
use crate::secrets::SecretCipher;
use crate::service::ProviderConnectionService;
use crate::usage_pools::{PostgresUsagePoolStore, UsagePool, UsagePoolStore};
use crate::usage_tracker::UsageSource;

const DEMO_RESEARCH_POOL: &str = "Usage demo · Research";
const DEMO_PRODUCT_POOL: &str = "Usage demo · Product";

struct DemoFixture {
    api_key_hash: String,
    usage_pool_id: String,
    connection_id: String,
    model: &'static str,
}

/// Seed representative usage for the Token Usage explorer. The supporting
/// connections are disabled and model routes unpublished, so the fixture never
/// accepts production traffic. It is safe to execute again: resources are
/// inserted by their distinctive names and only deterministic demo event ids
/// are refreshed with a recent range.
pub async fn run() -> Result<String, Box<dyn std::error::Error>> {
    let config = RouterConfig::from_env();
    let database_url = config
        .database_url
        .ok_or("seed-usage: set ROUTER_DATABASE_URL")?;
    let connect_options = crate::database::connect_options(&database_url)?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect_with(connect_options)
        .await?;

    let environment_id = config.environment_id;
    let usage_pools = PostgresUsagePoolStore::new(pool.clone(), environment_id.clone());
    let api_keys =
        PostgresApiKeyStore::new(pool.clone(), KeyHasher::from_env()?, environment_id.clone());
    let connection_store = Arc::new(PostgresProviderConnectionStore::new(
        pool.clone(),
        SecretCipher::from_env()?,
        environment_id.clone(),
    ));
    let connection_service =
        ProviderConnectionService::new(connection_store.clone(), reqwest::Client::new());
    let routes = PostgresModelRouteStore::new(pool.clone(), environment_id.clone());

    // Batched existence checks: one list query per resource type up front,
    // then in-memory lookups below, rather than a point-lookup query per demo
    // entry (mirrors the pattern in `seed.rs::apply`).
    let existing_pools: HashMap<String, UsagePool> = usage_pools
        .list_pools()
        .await
        .into_iter()
        .map(|p| (p.name.clone(), p))
        .collect();
    let existing_keys: HashMap<String, Arc<ApiKey>> = api_keys
        .list()
        .await
        .into_iter()
        .map(|k| (k.description.clone(), k))
        .collect();
    let existing_connections: HashMap<String, StoredConnection> = connection_service
        .list()
        .await?
        .into_iter()
        .map(|c| (c.name().to_string(), c))
        .collect();
    let existing_routes: HashMap<String, ModelRoute> = routes
        .list()
        .await?
        .into_iter()
        .map(|r| (r.public_model_id.clone(), r))
        .collect();

    let research_pool = ensure_pool(&usage_pools, &existing_pools, DEMO_RESEARCH_POOL).await?;
    let product_pool = ensure_pool(&usage_pools, &existing_pools, DEMO_PRODUCT_POOL).await?;

    let research_key = ensure_api_key(
        &api_keys,
        &existing_keys,
        "Usage demo · Research key",
        HashMap::from([
            ("team".to_string(), "research".to_string()),
            ("surface".to_string(), "playground".to_string()),
        ]),
        Some(research_pool.id.clone()),
    )
    .await?;
    let product_key = ensure_api_key(
        &api_keys,
        &existing_keys,
        "Usage demo · Product key",
        HashMap::from([
            ("team".to_string(), "product".to_string()),
            ("surface".to_string(), "api".to_string()),
        ]),
        Some(product_pool.id.clone()),
    )
    .await?;
    let platform_key = ensure_api_key(
        &api_keys,
        &existing_keys,
        "Usage demo · Platform key",
        HashMap::from([
            ("team".to_string(), "platform".to_string()),
            ("surface".to_string(), "batch".to_string()),
        ]),
        Some(product_pool.id.clone()),
    )
    .await?;

    let openai_connection = ensure_connection(
        &connection_service,
        &existing_connections,
        "Usage demo · OpenAI",
        "openai",
        None,
    )
    .await?;
    let anthropic_connection = ensure_connection(
        &connection_service,
        &existing_connections,
        "Usage demo · Anthropic",
        "anthropic",
        None,
    )
    .await?;
    let local_connection = ensure_connection(
        &connection_service,
        &existing_connections,
        "Usage demo · Local",
        "vllm",
        Some("http://127.0.0.1:9903/v1"),
    )
    .await?;

    let gpt_route = ensure_route(
        &routes,
        &existing_routes,
        "chalk/usage-demo-gpt-5",
        &openai_connection,
        "gpt-5",
    )
    .await?;
    let claude_route = ensure_route(
        &routes,
        &existing_routes,
        "chalk/usage-demo-claude-sonnet",
        &anthropic_connection,
        "claude-sonnet-4-5",
    )
    .await?;
    let local_route = ensure_route(
        &routes,
        &existing_routes,
        "chalk/usage-demo-local",
        &local_connection,
        "llama-3.3-70b-instruct",
    )
    .await?;

    let fixtures = [
        DemoFixture {
            api_key_hash: research_key,
            usage_pool_id: research_pool.id,
            connection_id: gpt_route.connection_id,
            model: "gpt-5",
        },
        DemoFixture {
            api_key_hash: product_key,
            usage_pool_id: product_pool.id.clone(),
            connection_id: claude_route.connection_id,
            model: "claude-sonnet-4-5",
        },
        DemoFixture {
            api_key_hash: platform_key,
            usage_pool_id: product_pool.id,
            connection_id: local_route.connection_id,
            model: "llama-3.3-70b-instruct",
        },
    ];

    let mut seeded_events = 0usize;
    let now = Utc::now();
    for day in 0..14usize {
        for (fixture_index, fixture) in fixtures.iter().enumerate() {
            let offset_hours = 3 + i64::try_from(fixture_index)? * 2;
            let time = now - Duration::days(i64::try_from(day)?) - Duration::hours(offset_hours);
            insert_event(&pool, &environment_id, time, fixture, day, fixture_index).await?;
            seeded_events += 1;
        }
    }

    Ok(format!(
        "seeded {seeded_events} demo usage event(s); \
         ensured 2 usage pools, 3 API keys, 3 disabled connections, and 3 unpublished routes"
    ))
}

async fn ensure_pool(
    store: &PostgresUsagePoolStore,
    existing: &HashMap<String, UsagePool>,
    name: &str,
) -> Result<UsagePool, Box<dyn std::error::Error>> {
    if let Some(pool) = existing.get(name) {
        return Ok(pool.clone());
    }
    Ok(store.create_pool(name.to_string()).await?)
}

async fn ensure_api_key(
    store: &PostgresApiKeyStore,
    existing: &HashMap<String, Arc<ApiKey>>,
    description: &str,
    labels: HashMap<String, String>,
    usage_pool_id: Option<String>,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(key) = existing.get(description) {
        return Ok(key.key_hash.clone());
    }
    let created = store
        .create(
            description.to_string(),
            ApiKeyPermission::default(),
            labels,
            usage_pool_id,
        )
        .await?;
    Ok(created.api_key.key_hash.clone())
}

async fn ensure_connection(
    service: &ProviderConnectionService,
    existing: &HashMap<String, StoredConnection>,
    name: &str,
    provider_kind: &str,
    base_url: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(connection) = existing.get(name) {
        return Ok(connection.id().to_string());
    }
    let connection = service
        .create(ConnectionInput {
            name: name.to_string(),
            provider_kind: provider_kind.to_string(),
            base_url: base_url.map(str::to_string),
            prefix: None,
            exposure: ExposurePolicy::Explicit,
            routing_enabled: false,
            declared_capabilities: None,
            api_key: KEYLESS_PLACEHOLDER.to_string(),
        })
        .await?;
    Ok(connection.id)
}

async fn ensure_route(
    store: &PostgresModelRouteStore,
    existing: &HashMap<String, ModelRoute>,
    public_model_id: &str,
    connection_id: &str,
    upstream_model_id: &str,
) -> Result<ModelRoute, Box<dyn std::error::Error>> {
    if let Some(route) = existing.get(public_model_id) {
        return Ok(route.clone());
    }
    Ok(store
        .create(RouteInput {
            routing: Default::default(),
            public_model_id: public_model_id.to_string(),
            connection_id: connection_id.to_string(),
            upstream_model_id: upstream_model_id.to_string(),
            published: false,
        })
        .await?)
}

async fn insert_event(
    pool: &sqlx::PgPool,
    environment_id: &str,
    time: chrono::DateTime<Utc>,
    fixture: &DemoFixture,
    day: usize,
    fixture_index: usize,
) -> Result<(), sqlx::Error> {
    let day = i64::try_from(day).expect("fixed demo range fits i64");
    let fixture_index = i64::try_from(fixture_index).expect("fixed fixture count fits i64");
    let prompt_tokens = 30_000 + day * 1_700 + fixture_index * 11_000;
    let completion_tokens = 7_000 + day * 450 + fixture_index * 2_300;
    let cached_tokens = prompt_tokens / (5 + fixture_index);
    let cache_write_tokens = prompt_tokens / (12 + fixture_index);
    let reasoning_tokens = completion_tokens / (4 + fixture_index);
    let event_id = uuid::Uuid::from_u128(
        0x7f41_942b_10fe_4d06_a14c_2b0b_0000_0000 + (day as u128) * 16 + fixture_index as u128,
    );

    sqlx::query(
        "INSERT INTO chalk_router.usage_events \
         (event_id, time, environment_id, api_key_hash, usage_pool_id, connection_id, \
          model, prompt_tokens, completion_tokens, cached_tokens, cache_write_tokens, \
          reasoning_tokens, total_tokens, request_count, usage_source) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, 1, $14) \
         ON CONFLICT (environment_id, event_id) DO UPDATE SET \
           time = EXCLUDED.time, \
           api_key_hash = EXCLUDED.api_key_hash, \
           usage_pool_id = EXCLUDED.usage_pool_id, \
           connection_id = EXCLUDED.connection_id, \
           model = EXCLUDED.model, \
           prompt_tokens = EXCLUDED.prompt_tokens, \
           completion_tokens = EXCLUDED.completion_tokens, \
           cached_tokens = EXCLUDED.cached_tokens, \
           cache_write_tokens = EXCLUDED.cache_write_tokens, \
           reasoning_tokens = EXCLUDED.reasoning_tokens, \
           total_tokens = EXCLUDED.total_tokens, \
           request_count = EXCLUDED.request_count, \
           usage_source = EXCLUDED.usage_source",
    )
    .bind(event_id)
    .bind(time)
    .bind(environment_id)
    .bind(&fixture.api_key_hash)
    .bind(&fixture.usage_pool_id)
    .bind(&fixture.connection_id)
    .bind(fixture.model)
    .bind(prompt_tokens)
    .bind(completion_tokens)
    .bind(cached_tokens)
    .bind(cache_write_tokens)
    .bind(reasoning_tokens)
    .bind(prompt_tokens + completion_tokens)
    .bind(UsageSource::Reported.as_db_code())
    .execute(pool)
    .await?;
    Ok(())
}
