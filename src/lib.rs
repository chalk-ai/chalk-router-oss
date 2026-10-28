pub mod adapters;
pub mod api_keys;
pub mod backend;
pub mod background_responses;
pub mod budget;
pub mod capabilities;
pub mod capability_discovery;
pub(crate) mod capability_requirements;
pub mod cluster_auth;
pub mod cluster_lifecycle;
pub mod config;
pub mod database;
pub(crate) mod db_error;
pub mod decisions;
pub mod egress;
pub(crate) mod embedding_limits;
pub(crate) mod embedding_quota;
pub mod environment_directory;
pub mod environment_runtime;
pub mod fallback;
pub mod grpc;
pub mod handlers;
pub mod hashing;
pub mod jev_prompting;
pub mod judge;
pub mod judge_policy;
pub mod labels;
pub mod metadata_client;
pub mod middleware;
pub mod migrate;
pub mod model_capability_registry;
pub mod model_routes;
pub mod native_audio;
pub mod native_chat;
pub mod native_json;
pub mod pricing;
pub mod prompt_cache;
pub mod provider_connections;
pub mod provider_registry;
pub mod quota;
pub mod quota_definition_cache;
pub mod quota_policy;
pub mod ratelimit;
pub mod response_locator;
pub mod responses;
pub mod revisioned_policy;
pub mod route_grid;
pub mod sampling_policy;
pub mod schema_json;
pub mod secrets;
pub mod seed;
pub mod service;
pub mod storage;
pub mod structured_output;
pub mod telemetry;
pub mod telemetry_settings;
pub mod tenancy;
pub mod translation;
pub mod types;
pub(crate) mod usage_estimator;
pub mod usage_pools;
pub mod usage_seed;
pub mod usage_tracker;

use api_keys::ApiKeyStore;
use backend::BackendProvider;
use budget::BudgetPolicyStore;
use model_routes::ModelRouteStore;
use prompt_cache::PromptCache;
use provider_connections::ProviderConnectionStore;
use quota::QuotaBackend;
use quota_policy::QuotaPolicyResolver;
use ratelimit::RateLimitPolicyStore;
use sampling_policy::SamplingPolicyStore;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::Arc;
use storage::SharedObjectStore;
use telemetry_settings::TelemetrySettingsService;
use tokio::sync::RwLock;
use usage_pools::UsagePoolStore;
use usage_tracker::UsageTracker;

/// `User-Agent` on every request the router makes upstream. Some providers
/// reject requests without one: Snowflake Cortex's OpenAI-compatible API
/// answers 400 (code 391903, "Invalid or empty User-Agent header").
pub const UPSTREAM_USER_AGENT: &str = concat!("chalk-router/", env!("CARGO_PKG_VERSION"));

/// The outbound client for the operator's own environment. Every backend and
/// administrative probe receives a cheap clone backed by its connection pool;
/// environments the operator does not control use [`egress::guarded_client`].
pub fn upstream_client(connect_timeout: std::time::Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .user_agent(UPSTREAM_USER_AGENT)
        .build()
        .expect("outbound HTTP client")
}

/// Maximum number of debug entries to keep in the ring buffer
const MAX_DEBUG_ENTRIES: usize = 100;

/// A recorded chat completion call for debug inspection
#[derive(Debug, Clone, Serialize)]
pub struct DebugCompletionEntry {
    pub timestamp: String,
    pub model: String,
    pub message_count: usize,
    pub streaming: bool,
    pub status: String,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<types::Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Thread-safe ring buffer for debug entries.
///
/// Entries are stored behind an `Arc` so `recent()` only has to bump
/// refcounts while holding the read guard — the actual entry clones (each of
/// which owns a couple of `String`s and an `Option<Usage>`) happen after the
/// lock is released, instead of ~`MAX_DEBUG_ENTRIES` clones happening while
/// every writer on the chat hotpath is blocked on the write lock.
#[derive(Clone)]
pub struct DebugLog {
    entries: Arc<RwLock<VecDeque<Arc<DebugCompletionEntry>>>>,
}

impl Default for DebugLog {
    fn default() -> Self {
        Self::new()
    }
}

impl DebugLog {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(VecDeque::with_capacity(MAX_DEBUG_ENTRIES))),
        }
    }

    pub async fn push(&self, entry: DebugCompletionEntry) {
        let mut entries = self.entries.write().await;
        if entries.len() >= MAX_DEBUG_ENTRIES {
            entries.pop_front();
        }
        entries.push_back(Arc::new(entry));
    }

    pub async fn recent(&self) -> Vec<DebugCompletionEntry> {
        // Hold the read guard only long enough to clone the `Arc`s (pointer
        // bumps); the underlying `DebugCompletionEntry` clones happen after
        // it's dropped, off the lock.
        let arcs: Vec<Arc<DebugCompletionEntry>> = {
            let entries = self.entries.read().await;
            entries.iter().rev().cloned().collect()
        };
        arcs.iter().map(|e| (**e).clone()).collect()
    }
}

/// Application state shared across handlers.
///
/// Holds the raw dependencies (stores, clients, configs). HTTP and gRPC
/// entrypoints use the `*_service()` helpers below to construct the
/// stateless service-layer wrappers that own the actual business logic.
#[derive(Clone)]
/// Environment-bound runtime bundle. Cluster resources are shared through cheap
/// client/pool/Arc clones; tenant stores and mutable state belong to this bundle.
pub struct AppState {
    pub route_judge: Arc<judge::RouteJudgeRuntime>,
    pub backend: Arc<dyn BackendProvider>,
    pub http_client: reqwest::Client,
    pub debug_log: DebugLog,
    /// The chalk environment this deployment serves. Used to reject federated
    /// tokens whose `environment_id` claim names a different environment.
    pub environment_id: String,
    pub response_locator: response_locator::ResponseLocatorCodec,
    pub api_key_store: Arc<dyn ApiKeyStore>,
    pub usage_pool_store: Arc<dyn UsagePoolStore>,
    /// Trace sampling policy (see `crate::sampling_policy`). The span-export
    /// path reads it through the telemetry sampling gate, not from here; this
    /// handle exists for the admin surface.
    pub sampling_policy: Arc<dyn SamplingPolicyStore>,
    /// The same gate `main` wires into the tower layers, exposed here so a
    /// handler can check a response body against the policy's outcome rules
    /// (`crate::sampling_policy::OutcomeRule`) once it has the full response,
    /// without a second read of the store.
    pub sampling_gate: Arc<telemetry::sampling::SamplingGate>,
    pub usage_tracker: Option<UsageTracker>,
    pub background_responses: Arc<background_responses::BackgroundResponses>,
    /// Durable/in-memory control-plane definition stores.
    pub budget_store: Arc<dyn BudgetPolicyStore>,
    pub rate_limit_store: Arc<dyn RateLimitPolicyStore>,
    /// Compiles matching source definitions before mutable state is touched.
    pub quota_resolver: Arc<QuotaPolicyResolver>,
    /// The single mutable-state backend shared by budgets and rate limits.
    pub quota_backend: Arc<dyn QuotaBackend>,
    /// Exact-prefix completion cache (`prompt_cache`). Always present —
    /// `DisabledPromptCache` when no backend is configured — so the request
    /// path never branches on availability, only on whether the *key* opted in.
    pub prompt_cache: Arc<dyn PromptCache>,
    /// Deployment-level cache wiring: the TTL and entry-size ceiling a key's
    /// unset policy fields inherit. Read per request, so it lives in state
    /// rather than being re-read from the environment.
    pub prompt_cache_config: Arc<config::PromptCacheConfig>,
    pub object_store: SharedObjectStore,
    /// Provider connections (credentials + endpoints), set via the Providers UI
    /// or seeded into the DB. Shared with the RouterBackend, which resolves
    /// provider backends from it per request.
    pub provider_connection_store: Arc<dyn ProviderConnectionStore>,
    /// `chalk/*` model routes.
    pub model_route_store: Arc<dyn ModelRouteStore>,
    /// DB-backed optional telemetry exporter configuration.
    pub telemetry_settings: Arc<TelemetrySettingsService>,
    /// Trusted external JWT issuers (Chalk metadata API servers). A request
    /// (admin or proxied) carrying a JWT whose `iss` matches a configured
    /// issuer is authenticated against that issuer's JWKS — instead of
    /// `ROUTER_ADMIN_KEY` on the admin surface, or a stored API key on the proxied
    /// surface. Empty registry = federation off.
    pub federated_auth: Arc<chalk_federated_auth::OidcRegistry>,
    /// Owner of this environment for federated admin authorization. Cluster
    /// JWT routing uses the metadata directory instead of this static owner.
    pub federated_admin_team_id: Option<String>,
    /// Required audience for configured federation, including tokens omitting aud.
    pub federated_jwt_audience: Option<String>,
    /// Admin key for the admin surface (and the UI's proxied-access shortcut),
    /// resolved once at boot from `ROUTER_ADMIN_KEY` (empty/unset => `None`). Read from
    /// state rather than the environment on every request.
    pub admin_key: Option<String>,
    /// Explicit local-development opt-in for credential-free proxy requests.
    /// Never applies to admin routes or requests presenting invalid credentials.
    pub dangerously_permit_unauthenticated: bool,
    /// Whether a federated JWT whose `environment_id` claim names a different
    /// environment than this process may still authenticate. The shared
    /// go-api-server proxy stamps `X-Chalk-Env-Id` to this process's env
    /// (`demo`) while forwarding a JWT that still names the caller's
    /// environment. Per-environment routers leave this off.
    /// Singular-deployment compatibility only. Cluster authentication always
    /// resolves the real environment and rejects a conflicting selector.
    /// From `ROUTER_ACCEPT_CROSS_ENVIRONMENT_TOKENS`.
    pub accept_cross_environment_tokens: bool,
    /// Which process-level cloud identities this environment's provider
    /// connections may use (see [`provider_connections::ConnectionPolicy`]).
    pub connection_policy: provider_connections::ConnectionPolicy,
}

/// Build the ephemeral telemetry settings service used by tests and local
/// in-memory deployments that do not have a Postgres pool.
pub fn default_telemetry_settings_service() -> Arc<TelemetrySettingsService> {
    Arc::new(TelemetrySettingsService::new(
        Arc::new(telemetry_settings::InMemoryTelemetryExporterStore::new(
            telemetry_settings::default_telemetry_exporters(),
        )),
        telemetry::TelemetryRuntime::new(Vec::new()),
    ))
}

impl AppState {
    pub fn api_key_service(&self) -> service::ApiKeyService {
        service::ApiKeyService::new(
            self.api_key_store.clone(),
            self.usage_tracker.clone(),
            self.budget_store.clone(),
            self.rate_limit_store.clone(),
            self.provider_connection_store.clone(),
            self.model_route_store.clone(),
            self.quota_backend.enabled(),
        )
    }

    pub fn usage_pool_service(&self) -> service::UsagePoolService {
        service::UsagePoolService::new(
            self.usage_pool_store.clone(),
            self.budget_store.clone(),
            self.rate_limit_store.clone(),
            self.environment_id.clone(),
            self.quota_backend.enabled(),
        )
    }

    pub fn usage_budget_service(&self) -> service::UsageBudgetService {
        service::UsageBudgetService::new(
            self.budget_store.clone(),
            self.api_key_store.clone(),
            self.usage_pool_store.clone(),
            self.environment_id.clone(),
            self.quota_backend.enabled(),
            self.quota_backend.clone(),
        )
    }

    pub fn usage_service(&self) -> service::UsageService {
        service::UsageService::new(self.usage_tracker.clone(), self.api_key_store.clone())
    }

    pub fn rate_limit_service(&self) -> service::RateLimitService {
        service::RateLimitService::new(self.rate_limit_store.clone(), self.quota_backend.enabled())
    }

    pub fn sampling_service(&self) -> service::SamplingPolicyService {
        service::SamplingPolicyService::new(self.sampling_policy.clone())
    }

    pub fn provider_connection_service(&self) -> service::ProviderConnectionService {
        service::ProviderConnectionService::new(
            self.provider_connection_store.clone(),
            self.http_client.clone(),
        )
        .with_connection_policy(self.connection_policy)
    }

    pub fn model_route_service(&self) -> service::ModelRouteService {
        service::ModelRouteService::new(
            self.model_route_store.clone(),
            self.provider_connection_store.clone(),
        )
    }

    /// Who is asking, for cache-key scoping (see [`prompt_cache::CacheScope`]).
    fn cache_identity<'a>(
        &'a self,
        api_key: &'a api_keys::ApiKey,
    ) -> prompt_cache::CacheIdentity<'a> {
        prompt_cache::CacheIdentity {
            environment_id: &self.environment_id,
            usage_pool_id: api_key.usage_pool_id.as_deref(),
            key_hash: &api_key.key_hash,
        }
    }

    /// Cache context for an OpenAI-shaped chat request, or `None` when this
    /// deployment has no cache backend or this key did not opt in.
    pub fn chat_cache_context(
        &self,
        api_key: &api_keys::ApiKey,
        req: &types::ChatCompletionRequest,
    ) -> Option<prompt_cache::CacheContext> {
        let identity = self.cache_identity(api_key);
        prompt_cache::context_for(
            self.prompt_cache.as_ref(),
            api_key.permission.cache_policy(),
            &self.prompt_cache_config,
            &identity,
            |scope| prompt_cache::chat_cache_key(scope, &identity, req),
        )
    }

    pub fn native_chat_cache_context(
        &self,
        api_key: &api_keys::ApiKey,
        request: &native_chat::NativeChatRequest,
    ) -> Option<prompt_cache::CacheContext> {
        let identity = self.cache_identity(api_key);
        prompt_cache::context_for(
            self.prompt_cache.as_ref(),
            api_key.permission.cache_policy(),
            &self.prompt_cache_config,
            &identity,
            |scope| prompt_cache::native_chat_cache_key(scope, &identity, request),
        )
    }

    /// Cache context for a passthrough surface digested from its raw body
    /// (`/v1/messages`).
    ///
    /// `headers` are the client's request headers: this surface replays stored
    /// upstream bytes, so the headers that steer the response shape are part
    /// of the key (see [`prompt_cache::ResponseShapeHeaders`]).
    pub fn raw_body_cache_context(
        &self,
        api_key: &api_keys::ApiKey,
        surface: prompt_cache::Surface,
        headers: &http::HeaderMap,
        body: &[u8],
    ) -> Option<prompt_cache::CacheContext> {
        let identity = self.cache_identity(api_key);
        let shape = prompt_cache::ResponseShapeHeaders::from_headers(headers);
        prompt_cache::context_for(
            self.prompt_cache.as_ref(),
            api_key.permission.cache_policy(),
            &self.prompt_cache_config,
            &identity,
            |scope| {
                Some(prompt_cache::raw_body_cache_key(
                    scope, &identity, surface, &shape, body,
                ))
            },
        )
    }

    pub fn debug_service(&self) -> service::DebugService {
        service::DebugService::new(self.debug_log.clone())
    }

    pub fn background_response_service(&self) -> service::BackgroundResponseService {
        service::BackgroundResponseService::new(self.background_responses.clone())
    }

    pub fn telemetry_settings_service(&self) -> Arc<TelemetrySettingsService> {
        self.telemetry_settings.clone()
    }
}

/// Parse a `CHALK_JWT_ISSUERS` spec into [`chalk_federated_auth::IssuerConfig`]s.
///
/// Each comma-separated entry is one of:
///   - `https://issuer.example` — a trusted issuer whose JWKS is found via OIDC
///     discovery (`{issuer}/.well-known/openid-configuration`).
///   - `name=https://host/.well-known/jwks.json` — issuer `name` validated
///     against the explicit JWKS URI. Use this when the token's `iss` is a bare
///     string that isn't a URL (e.g. Chalk metadata-API user tokens carry
///     `iss="be"`), so discovery can't run.
///
/// `audience`, when set, pins the `aud` claim on every parsed issuer.
pub fn parse_chalk_jwt_issuers(
    spec: &str,
    audience: Option<&str>,
) -> Vec<chalk_federated_auth::IssuerConfig> {
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|entry| {
            // `split_once` on the first `=` keeps query strings in the JWKS URI
            // intact (only the issuer/URI boundary is split).
            let mut cfg = match entry.split_once('=') {
                Some((issuer, jwks_uri)) => {
                    let mut c = chalk_federated_auth::IssuerConfig::discovery(issuer.trim());
                    c.jwks_uri = Some(jwks_uri.trim().to_string());
                    c
                }
                None => chalk_federated_auth::IssuerConfig::discovery(entry),
            };
            if let Some(aud) = audience {
                cfg = cfg.with_audience(aud);
            }
            cfg
        })
        .collect()
}

#[cfg(test)]
mod upstream_client_tests {
    use super::{upstream_client, UPSTREAM_USER_AGENT};
    use axum::{http::HeaderMap, routing::get, Router};

    #[tokio::test]
    async fn upstream_requests_identify_the_router() {
        let app = Router::new().route(
            "/",
            get(|headers: HeaderMap| async move {
                headers
                    .get("user-agent")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_string()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = upstream_client(std::time::Duration::from_secs(2));
        let sent = client
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(sent, UPSTREAM_USER_AGENT);
        assert!(sent.starts_with("chalk-router/"), "{sent}");
    }
}

#[cfg(test)]
mod jwt_issuer_parsing_tests {
    use super::parse_chalk_jwt_issuers;

    #[test]
    fn url_entry_uses_discovery() {
        let issuers = parse_chalk_jwt_issuers("https://api.chalk.ai", None);
        assert_eq!(issuers.len(), 1);
        assert_eq!(issuers[0].issuer, "https://api.chalk.ai");
        assert_eq!(issuers[0].jwks_uri, None);
    }

    #[test]
    fn bare_issuer_with_jwks_uri() {
        let issuers = parse_chalk_jwt_issuers(
            "be=https://issuer.example.invalid/.well-known/jwks.json",
            None,
        );
        assert_eq!(issuers.len(), 1);
        assert_eq!(issuers[0].issuer, "be");
        assert_eq!(
            issuers[0].jwks_uri.as_deref(),
            Some("https://issuer.example.invalid/.well-known/jwks.json")
        );
    }

    #[test]
    fn mixed_entries_and_audience() {
        let issuers = parse_chalk_jwt_issuers(
            " https://api.chalk.ai , be=https://host/jwks?kid=1 ",
            Some("chalk-router"),
        );
        assert_eq!(issuers.len(), 2);
        assert_eq!(issuers[0].issuer, "https://api.chalk.ai");
        assert_eq!(issuers[0].jwks_uri, None);
        assert_eq!(issuers[0].audience.as_deref(), Some("chalk-router"));
        assert_eq!(issuers[1].issuer, "be");
        // Only the first `=` splits, so JWKS query strings survive.
        assert_eq!(
            issuers[1].jwks_uri.as_deref(),
            Some("https://host/jwks?kid=1")
        );
        assert_eq!(issuers[1].audience.as_deref(), Some("chalk-router"));
    }

    #[test]
    fn empty_spec_yields_no_issuers() {
        assert!(parse_chalk_jwt_issuers("", None).is_empty());
        assert!(parse_chalk_jwt_issuers("  ,  ", None).is_empty());
    }
}
