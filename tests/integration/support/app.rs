//! Shared `AppState` construction. Every integration test used to hand-build
//! its own 24-field `AppState { .. }` literal (27 of them, across 16 files);
//! `AppStateBuilder` holds the same in-memory defaults those literals all
//! duplicated and lets a test override only the fields it actually varies.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{extract::DefaultBodyLimit, middleware, routing::post, Router};
use openai_server::{
    api_keys::{ApiKey, ApiKeyPermission, ApiKeyStore, InMemoryApiKeyStore},
    backend::BackendProvider,
    budget::{self, BudgetPolicyStore},
    config,
    fallback::FallbackPolicy,
    handlers,
    hashing::{key_prefix, KeyHasher},
    middleware::auth_middleware,
    model_routes::{self, ModelRouteStore},
    prompt_cache::{self, PromptCache},
    provider_connections::{self, ProviderConnectionStore},
    quota::{self, QuotaBackend},
    quota_policy::QuotaPolicyResolver,
    ratelimit::{self, RateLimitPolicyStore},
    response_locator::ResponseLocatorCodec,
    sampling_policy::{InMemorySamplingPolicyStore, SamplingPolicy, SamplingPolicyStore},
    storage::SharedObjectStore,
    telemetry::sampling::SamplingGate,
    telemetry_settings::TelemetrySettingsService,
    usage_pools::{InMemoryUsagePoolStore, UsagePoolStore},
    usage_tracker::UsageTracker,
    AppState, DebugLog,
};

/// The key `AppStateBuilder`'s default `api_key_store` seeds, for tests that
/// only need auth to succeed and don't care about the key's identity.
pub const TEST_API_KEY: &str = "test-key-for-spans";

/// Builds an `AppState` from the same in-memory defaults every hand-written
/// test literal used: no judge, no usage tracker, quota/prompt-cache
/// disabled, federation off, no admin key, unauthenticated requests
/// rejected. Call `.build().await` last; it seeds `TEST_API_KEY` into the
/// default `api_key_store` unless `api_key_store()` replaced it.
pub struct AppStateBuilder {
    backend: Arc<dyn BackendProvider>,
    http_client: reqwest::Client,
    debug_log: DebugLog,
    environment_id: String,
    response_locator: ResponseLocatorCodec,
    api_key_store: Arc<dyn ApiKeyStore>,
    seed_test_api_key: bool,
    usage_pool_store: Arc<dyn UsagePoolStore>,
    sampling_policy: Arc<dyn SamplingPolicyStore>,
    sampling_gate: Arc<SamplingGate>,
    usage_tracker: Option<UsageTracker>,
    budget_store: Arc<dyn BudgetPolicyStore>,
    rate_limit_store: Arc<dyn RateLimitPolicyStore>,
    quota_resolver: Arc<QuotaPolicyResolver>,
    quota_backend: Arc<dyn QuotaBackend>,
    prompt_cache: Arc<dyn PromptCache>,
    prompt_cache_config: Arc<config::PromptCacheConfig>,
    object_store: SharedObjectStore,
    provider_connection_store: Arc<dyn ProviderConnectionStore>,
    model_route_store: Arc<dyn ModelRouteStore>,
    telemetry_settings: Arc<TelemetrySettingsService>,
    federated_auth: Arc<chalk_federated_auth::OidcRegistry>,
    admin_key: Option<String>,
    dangerously_permit_unauthenticated: bool,
    accept_cross_environment_tokens: bool,
}

// The builder exposes a setter for every `AppState` knob so a test can override
// exactly what it needs; not every setter has a caller at any given time.
#[allow(dead_code)]
impl AppStateBuilder {
    pub fn new(backend: Arc<dyn BackendProvider>) -> Self {
        let hasher = KeyHasher::new(b"test-pepper".to_vec());
        let api_key_store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(hasher));
        Self {
            backend,
            http_client: reqwest::Client::new(),
            debug_log: DebugLog::new(),
            environment_id: "develop".to_string(),
            response_locator: ResponseLocatorCodec::new(b"test-locator".to_vec()),
            api_key_store,
            seed_test_api_key: true,
            usage_pool_store: Arc::new(InMemoryUsagePoolStore::new()),
            sampling_policy: Arc::new(InMemorySamplingPolicyStore::new(SamplingPolicy::default())),
            sampling_gate: Arc::new(SamplingGate::new()),
            usage_tracker: None,
            budget_store: budget::from_core_store(None, "test".to_string()),
            rate_limit_store: ratelimit::from_core_store(None, "test".to_string()),
            quota_resolver: QuotaPolicyResolver::in_memory("test".to_string()),
            quota_backend: quota::from_kind(config::QuotaBackendKind::Disabled, None),
            prompt_cache: Arc::new(prompt_cache::DisabledPromptCache),
            prompt_cache_config: Arc::new(config::PromptCacheConfig::disabled()),
            object_store: None,
            provider_connection_store: Arc::new(
                provider_connections::InMemoryProviderConnectionStore::new(),
            ),
            model_route_store: Arc::new(model_routes::InMemoryModelRouteStore::new()),
            telemetry_settings: openai_server::default_telemetry_settings_service(),
            federated_auth: Arc::new(chalk_federated_auth::OidcRegistry::empty()),
            admin_key: None,
            dangerously_permit_unauthenticated: false,
            accept_cross_environment_tokens: false,
        }
    }

    /// Compatibility helper for older fallback tests: FixtureGridBackend attaches
    /// the supplied alternatives to each resolved route's RouteGrid. Request
    /// routing uses that grid; this helper does not configure a global policy.
    pub fn fallback_policy(mut self, policy: FallbackPolicy) -> Self {
        if !policy.as_map().is_empty() {
            self.backend = Arc::new(FixtureGridBackend {
                inner: self.backend,
                policy,
            });
        }
        self
    }

    pub fn sampling_policy_store(mut self, store: Arc<dyn SamplingPolicyStore>) -> Self {
        self.sampling_policy = store;
        self
    }

    pub fn federated_auth(
        mut self,
        federated_auth: Arc<chalk_federated_auth::OidcRegistry>,
    ) -> Self {
        self.federated_auth = federated_auth;
        self
    }

    pub fn admin_key(mut self, admin_key: impl Into<String>) -> Self {
        self.admin_key = Some(admin_key.into());
        self
    }

    pub fn accept_cross_environment_tokens(mut self, accept: bool) -> Self {
        self.accept_cross_environment_tokens = accept;
        self
    }

    pub fn dangerously_permit_unauthenticated(mut self, permit: bool) -> Self {
        self.dangerously_permit_unauthenticated = permit;
        self
    }

    pub fn environment_id(mut self, environment_id: impl Into<String>) -> Self {
        self.environment_id = environment_id.into();
        self
    }

    /// Replace the api-key store. Disables the default `TEST_API_KEY` seed —
    /// call `seed_test_api_key()` too if the replacement store still needs it.
    pub fn api_key_store(mut self, store: Arc<dyn ApiKeyStore>) -> Self {
        self.api_key_store = store;
        self.seed_test_api_key = false;
        self
    }

    pub fn seed_test_api_key(mut self, seed: bool) -> Self {
        self.seed_test_api_key = seed;
        self
    }

    pub fn usage_pool_store(mut self, store: Arc<dyn UsagePoolStore>) -> Self {
        self.usage_pool_store = store;
        self
    }

    pub fn usage_tracker(mut self, tracker: UsageTracker) -> Self {
        self.usage_tracker = Some(tracker);
        self
    }

    pub fn budget_store(mut self, store: Arc<dyn BudgetPolicyStore>) -> Self {
        self.budget_store = store;
        self
    }

    pub fn rate_limit_store(mut self, store: Arc<dyn RateLimitPolicyStore>) -> Self {
        self.rate_limit_store = store;
        self
    }

    pub fn quota_backend(mut self, backend: Arc<dyn QuotaBackend>) -> Self {
        self.quota_backend = backend;
        self
    }

    pub fn quota_resolver(mut self, resolver: Arc<QuotaPolicyResolver>) -> Self {
        self.quota_resolver = resolver;
        self
    }

    pub fn prompt_cache(mut self, cache: Arc<dyn PromptCache>) -> Self {
        self.prompt_cache = cache;
        self
    }

    pub fn prompt_cache_config(mut self, cfg: Arc<config::PromptCacheConfig>) -> Self {
        self.prompt_cache_config = cfg;
        self
    }

    pub fn object_store(mut self, store: SharedObjectStore) -> Self {
        self.object_store = store;
        self
    }

    pub fn provider_connection_store(mut self, store: Arc<dyn ProviderConnectionStore>) -> Self {
        self.provider_connection_store = store;
        self
    }

    pub fn model_route_store(mut self, store: Arc<dyn ModelRouteStore>) -> Self {
        self.model_route_store = store;
        self
    }

    pub fn telemetry_settings(mut self, settings: Arc<TelemetrySettingsService>) -> Self {
        self.telemetry_settings = settings;
        self
    }

    pub fn debug_log(mut self, debug_log: DebugLog) -> Self {
        self.debug_log = debug_log;
        self
    }

    pub fn http_client(mut self, client: reqwest::Client) -> Self {
        self.http_client = client;
        self
    }

    pub fn response_locator(mut self, locator: ResponseLocatorCodec) -> Self {
        self.response_locator = locator;
        self
    }

    /// Materialize the `AppState`, seeding `TEST_API_KEY` into the
    /// `api_key_store` when `seed_test_api_key` is still set (the default,
    /// unless `api_key_store()` turned it off).
    pub async fn build(self) -> AppState {
        // Point the gate at whatever `sampling_policy` store this builder
        // resolved to (the default, or a test's `sampling_policy_store()`
        // override) and prime it, so `state.sampling_gate.matched_outcome_rate`
        // sees the same policy `state.sampling_policy` would read.
        self.sampling_gate.set_store(self.sampling_policy.clone());
        self.sampling_gate.refresh().await;

        if self.seed_test_api_key {
            let hasher = KeyHasher::new(b"test-pepper".to_vec());
            self.api_key_store
                .insert_raw(ApiKey {
                    key_hash: hasher.hash(TEST_API_KEY),
                    key_prefix: key_prefix(TEST_API_KEY),
                    description: "test key".to_string(),
                    created_at: chrono::Utc::now(),
                    permission: ApiKeyPermission::default(),
                    labels: HashMap::new(),
                    usage_pool_id: None,
                    daily_token_budget: None,
                })
                .await
                .expect("seed test key");
        }

        AppState {
            route_judge: Default::default(),
            backend: self.backend,
            http_client: self.http_client,
            debug_log: self.debug_log,
            environment_id: self.environment_id.clone(),
            response_locator: self.response_locator,
            api_key_store: self.api_key_store,
            usage_pool_store: self.usage_pool_store,
            sampling_policy: self.sampling_policy,
            sampling_gate: self.sampling_gate,
            usage_tracker: self.usage_tracker,
            background_responses: Arc::new(
                openai_server::background_responses::BackgroundResponses::new(
                    None,
                    self.environment_id.clone(),
                    Default::default(),
                ),
            ),
            budget_store: self.budget_store,
            rate_limit_store: self.rate_limit_store,
            quota_resolver: self.quota_resolver,
            quota_backend: self.quota_backend,
            prompt_cache: self.prompt_cache,
            prompt_cache_config: self.prompt_cache_config,
            object_store: self.object_store,
            provider_connection_store: self.provider_connection_store,
            model_route_store: self.model_route_store,
            telemetry_settings: self.telemetry_settings,
            federated_auth: self.federated_auth,
            federated_admin_team_id: None,
            federated_jwt_audience: None,
            admin_key: self.admin_key,
            dangerously_permit_unauthenticated: self.dangerously_permit_unauthenticated,
            accept_cross_environment_tokens: self.accept_cross_environment_tokens,
            connection_policy: openai_server::provider_connections::ConnectionPolicy::trusted(),
        }
    }
}

fn chat_only_router(state: AppState) -> Router {
    Router::new()
        .route(
            "/v1/chat/completions",
            post(handlers::chat::create_chat_completion),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(DefaultBodyLimit::max(100 * 1024 * 1024))
        .with_state(state)
}

/// Chat-completions-only app behind auth middleware, everything else at
/// `AppStateBuilder` defaults.
pub async fn build_app(backend: Arc<dyn BackendProvider>) -> Router {
    build_app_with_fallback_policy(backend, FallbackPolicy::empty()).await
}

pub async fn build_app_with_fallback_policy(
    backend: Arc<dyn BackendProvider>,
    fallback_policy: FallbackPolicy,
) -> Router {
    let state = AppStateBuilder::new(backend)
        .fallback_policy(fallback_policy)
        .build()
        .await;
    chat_only_router(state)
}

/// Chat-completions-only app with a caller-supplied sampling policy store,
/// otherwise at `AppStateBuilder` defaults — for tests exercising outcome
/// rules or rate-based sampling against real chat-completion traffic.
pub async fn build_app_with_sampling_policy(
    backend: Arc<dyn BackendProvider>,
    sampling_policy: Arc<dyn SamplingPolicyStore>,
) -> Router {
    let state = AppStateBuilder::new(backend)
        .sampling_policy_store(sampling_policy)
        .build()
        .await;
    chat_only_router(state)
}

pub async fn build_app_with_federated_auth(
    backend: Arc<dyn BackendProvider>,
    fallback_policy: FallbackPolicy,
    federated_auth: Arc<chalk_federated_auth::OidcRegistry>,
) -> Router {
    let state = AppStateBuilder::new(backend)
        .fallback_policy(fallback_policy)
        .federated_auth(federated_auth)
        .build()
        .await;
    chat_only_router(state)
}

/// The full proxied surface (chat, embeddings, images, audio) behind auth
/// middleware, at `AppStateBuilder` defaults.
pub async fn build_full_app(backend: Arc<dyn BackendProvider>) -> Router {
    let state = AppStateBuilder::new(backend).build().await;
    Router::new()
        .route(
            "/v1/chat/completions",
            post(handlers::chat::create_chat_completion),
        )
        .route(
            "/v1/embeddings",
            post(handlers::embeddings::create_embedding),
        )
        .route(
            "/v1/images/generations",
            post(handlers::images::create_image),
        )
        .route(
            "/v1/audio/transcriptions",
            post(handlers::audio::create_transcription),
        )
        .route(
            "/v1/audio/translations",
            post(handlers::audio::create_translation),
        )
        .route("/v1/audio/speech", post(handlers::audio::create_speech))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(DefaultBodyLimit::max(100 * 1024 * 1024))
        .with_state(state)
}

pub fn chat_request_body(stream: bool) -> serde_json::Value {
    serde_json::json!({
        "model": "mock-model",
        "messages": [{"role": "user", "content": "Hello"}],
        "stream": stream,
    })
}

/// Existing protocol fixtures describe leaf-model recovery with a map. Attach
/// those fixtures to resolved routes; production never reads a global policy.
struct FixtureGridBackend {
    inner: Arc<dyn openai_server::backend::BackendProvider>,
    policy: FallbackPolicy,
}
impl FixtureGridBackend {
    fn attach(
        &self,
        model: &str,
        mut route: openai_server::backend::ResolvedRoute,
    ) -> openai_server::backend::ResolvedRoute {
        route.routing = Some(Arc::new(openai_server::route_grid::RouteGrid {
            default_target: openai_server::route_grid::RouteTarget {
                connection_id: route
                    .resolution
                    .connection_id
                    .clone()
                    .unwrap_or_else(|| route.resolution.provider_kind.clone()),
                upstream_model_id: route.resolution.upstream_model_id.clone(),
            },
            fallbacks: std::collections::BTreeMap::from([(
                route.concrete_model(),
                self.policy.fallbacks_for(model).to_vec(),
            )]),
            judge: None,
        }));
        route
    }
}
#[async_trait::async_trait]
impl openai_server::backend::BackendProvider for FixtureGridBackend {
    async fn list_models(
        &self,
    ) -> Result<
        openai_server::types::ListResponse<openai_server::types::Model>,
        openai_server::backend::BackendError,
    > {
        self.inner.list_models().await
    }
    async fn get_model(
        &self,
        model: &str,
    ) -> Result<openai_server::types::Model, openai_server::backend::BackendError> {
        self.inner.get_model(model).await
    }

    fn name(&self) -> &str {
        self.inner.name()
    }
    async fn resolve(
        &self,
        model: &str,
    ) -> Result<openai_server::backend::ResolvedRoute, openai_server::backend::BackendError> {
        Ok(self.attach(model, self.inner.resolve(model).await?))
    }
    async fn resolve_anthropic(
        &self,
        model: &str,
    ) -> Result<openai_server::backend::ResolvedRoute, openai_server::backend::BackendError> {
        Ok(self.attach(model, self.inner.resolve_anthropic(model).await?))
    }
    async fn resolve_strict(
        &self,
        model: &str,
    ) -> Result<openai_server::backend::ResolvedRoute, openai_server::backend::BackendError> {
        Ok(self.attach(model, self.inner.resolve_strict(model).await?))
    }
    async fn resolve_concrete(
        &self,
        model: &str,
    ) -> Result<openai_server::backend::ResolvedRoute, openai_server::backend::BackendError> {
        // Legacy fixture policies contain public model IDs and even bare leaf
        // names. Real recorder fixtures use stored connection UUIDs instead.
        if model
            .split_once('/')
            .is_some_and(|(connection, _)| uuid::Uuid::parse_str(connection).is_ok())
        {
            self.inner.resolve_concrete(model).await
        } else {
            self.inner.resolve_strict(model).await
        }
    }
    async fn resolve_connection(
        &self,
        id: &str,
    ) -> Result<openai_server::backend::ResolvedRoute, openai_server::backend::BackendError> {
        self.inner.resolve_connection(id).await
    }
    async fn resolve_default(
        &self,
    ) -> Result<openai_server::backend::ResolvedRoute, openai_server::backend::BackendError> {
        self.inner.resolve_default().await
    }
}
