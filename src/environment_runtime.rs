//! Construct isolated tenant state from shared cluster infrastructure.
use std::{collections::HashMap, sync::Arc, time::Duration};

use futures::{stream, StreamExt};

use crate::{
    api_keys::{ApiKeyStore, InMemoryApiKeyStore, PostgresApiKeyStore},
    backend::RouterBackend,
    cluster_lifecycle::BackgroundWorkers,
    config::{RouterConfig, UsageBackendKind},
    environment_directory::EnvironmentDirectory,
    hashing::KeyHasher,
    model_routes::{InMemoryModelRouteStore, ModelRouteStore, PostgresModelRouteStore},
    provider_connections::{
        ConnectionPolicy, InMemoryProviderConnectionStore, PostgresProviderConnectionStore,
        ProviderConnectionStore,
    },
    sampling_policy::{
        InMemorySamplingPolicyStore, PostgresSamplingPolicyStore, SamplingPolicy,
        SamplingPolicyStore,
    },
    secrets::SecretCipher,
    telemetry::TelemetryRuntime,
    telemetry_settings::{
        InMemoryTelemetryExporterStore, PostgresTelemetryExporterStore, TelemetrySettingsService,
        TelemetrySettingsStore,
    },
    tenancy::EnvironmentRegistry,
    usage_pools::{InMemoryUsagePoolStore, PostgresUsagePoolStore, UsagePoolStore},
    AppState, DebugLog,
};

pub struct RuntimeFactory {
    pub shared: AppState,
    pub pool: Option<sqlx::PgPool>,
    pub hasher: KeyHasher,
    pub cipher: Option<SecretCipher>,
    pub config: RouterConfig,
}

impl RuntimeFactory {
    pub async fn build(&self, environment: &str) -> Result<(AppState, TelemetryRuntime), String> {
        let environment_id = environment.to_owned();
        let defaults = if self.config.legacy_environment_id.as_deref() == Some(environment) {
            crate::telemetry_settings::default_telemetry_exporters()
        } else {
            Vec::new()
        };
        #[allow(clippy::type_complexity)]
        let (keys, pools, connections, routes, sampling, exporters): (
            Arc<dyn ApiKeyStore>,
            Arc<dyn UsagePoolStore>,
            Arc<dyn ProviderConnectionStore>,
            Arc<dyn ModelRouteStore>,
            Arc<dyn SamplingPolicyStore>,
            Arc<dyn TelemetrySettingsStore>,
        ) = match &self.pool {
            Some(pool) => {
                let cipher = self
                    .cipher
                    .clone()
                    .ok_or("cluster KEK is required for persistent runtimes")?;
                (
                    Arc::new(PostgresApiKeyStore::new(
                        pool.clone(),
                        self.hasher.clone(),
                        environment_id.clone(),
                    )),
                    Arc::new(PostgresUsagePoolStore::new(
                        pool.clone(),
                        environment_id.clone(),
                    )),
                    Arc::new(PostgresProviderConnectionStore::new(
                        pool.clone(),
                        cipher.clone(),
                        environment_id.clone(),
                    )),
                    Arc::new(PostgresModelRouteStore::new(
                        pool.clone(),
                        environment_id.clone(),
                    )),
                    Arc::new(PostgresSamplingPolicyStore::new(
                        pool.clone(),
                        environment_id.clone(),
                    )),
                    // Environment-default Braintrust credentials belong to the legacy
                    // environment, never to newly onboarded tenants.
                    Arc::new(PostgresTelemetryExporterStore::new(
                        pool.clone(),
                        environment_id.clone(),
                        cipher,
                        defaults,
                    )),
                )
            }
            None => {
                let routes: Arc<dyn ModelRouteStore> = Arc::new(InMemoryModelRouteStore::new());
                (
                    Arc::new(InMemoryApiKeyStore::new(self.hasher.clone())),
                    Arc::new(InMemoryUsagePoolStore::new()),
                    Arc::new(InMemoryProviderConnectionStore::with_routes(routes.clone())),
                    routes,
                    Arc::new(InMemorySamplingPolicyStore::new(SamplingPolicy::default())),
                    Arc::new(InMemoryTelemetryExporterStore::new(defaults)),
                )
            }
        };
        // Provider metadata is tenant-owned configuration. Read it on demand
        // through the tenant backend: a malformed row must fail that tenant's
        // operation, not prevent every other environment from starting.
        let telemetry = TelemetryRuntime::new(Vec::new());
        let connection_policy = ConnectionPolicy::for_environment(&self.config, environment);
        // Exporter destinations are tenant-chosen too; set before the first
        // refresh applies any stored destination.
        telemetry.set_public_egress_only(connection_policy.public_egress_only());
        telemetry.set_sampling_policy_store(sampling.clone());
        telemetry.set_sampling_secret(self.hasher.sampling_secret_for_environment(environment));
        telemetry.prime_sampling_policy().await;
        let settings = Arc::new(TelemetrySettingsService::new(exporters, telemetry.clone()));
        // Failure to read optional exporters can be retried by the cluster
        // refresh loop; it must never fall back to another tenant's settings.
        if let Err(error) = settings.refresh().await {
            tracing::warn!(environment_id = %environment, %error, "tenant telemetry settings unavailable");
        }
        let budget_store =
            crate::budget::from_core_store(self.pool.clone(), environment_id.clone());
        let rate_limit_store =
            crate::ratelimit::from_core_store(self.pool.clone(), environment_id.clone());
        let usage_tracker = if self.config.usage_backend == UsageBackendKind::Postgres {
            match &self.pool {
                Some(pool) => Some(
                    crate::usage_tracker::UsageTracker::new(pool.clone(), environment_id.clone())
                        .await
                        .map_err(|error| error.to_string())?,
                ),
                None => None,
            }
        } else {
            None
        };
        let mut state = self.shared.clone();
        state.environment_id = environment_id.clone();
        state.route_judge = Default::default();
        state.debug_log = DebugLog::new();
        state.connection_policy = connection_policy;
        if state.connection_policy.public_egress_only() {
            // Every outbound request made for an environment the operator does
            // not control (provider calls, probes, image fetches) goes through
            // the client that refuses private destinations.
            state.http_client = crate::egress::guarded_client(self.config.upstream_connect_timeout);
        }
        state.backend = Arc::new(
            RouterBackend::new(
                state.http_client.clone(),
                connections.clone(),
                routes.clone(),
            )
            .with_connection_policy(state.connection_policy),
        );
        state.api_key_store = keys;
        state.usage_pool_store = pools;
        state.provider_connection_store = connections;
        state.model_route_store = routes;
        state.sampling_policy = sampling;
        state.sampling_gate = telemetry.sampling_gate();
        state.telemetry_settings = settings;
        state.usage_tracker = usage_tracker;
        state.quota_resolver = Arc::new(crate::quota_policy::QuotaPolicyResolver::new(
            environment_id.clone(),
            budget_store.clone(),
            rate_limit_store.clone(),
        ));
        state.budget_store = budget_store;
        state.rate_limit_store = rate_limit_store;
        // Memory quota maps use local policy IDs, so instantiate one per tenant.
        // Postgres instances share only the connection pool and persisted UUIDs.
        state.quota_backend = crate::quota::from_kind(self.config.quota_backend, self.pool.clone());
        state.background_responses =
            Arc::new(crate::background_responses::BackgroundResponses::new(
                self.pool.clone(),
                environment_id,
                self.config.background_responses,
            ));
        state = state
            .bind_environment_resources(&self.hasher, self.config.legacy_environment_id.as_deref());
        Ok((state, telemetry))
    }
}

/// Loads cluster environments on first use. Callers confirm the environment
/// with the directory first (or, for the legacy environment, with trusted
/// configuration); provisioning builds the runtime, registers its telemetry
/// destinations, starts its background worker and publishes it.
pub struct RuntimeProvisioner {
    factory: RuntimeFactory,
    registry: Arc<EnvironmentRegistry>,
    telemetry: TelemetryRuntime,
    workers: Arc<BackgroundWorkers>,
    building: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl RuntimeProvisioner {
    pub fn new(
        factory: RuntimeFactory,
        registry: Arc<EnvironmentRegistry>,
        telemetry: TelemetryRuntime,
        workers: Arc<BackgroundWorkers>,
    ) -> Self {
        Self {
            factory,
            registry,
            telemetry,
            workers,
            building: Default::default(),
        }
    }

    /// The loaded runtime, building it once if needed. Concurrent first
    /// requests for one environment share a single build; a failed build is
    /// not remembered, so the next request retries.
    pub async fn get_or_provision(&self, environment: &str) -> Result<AppState, String> {
        if let Some(state) = self.registry.get(environment) {
            return Ok(state);
        }
        let gate = self
            .building
            .lock()
            .await
            .entry(environment.to_owned())
            .or_default()
            .clone();
        let _building = gate.lock().await;
        if let Some(state) = self.registry.get(environment) {
            return Ok(state);
        }
        let result = self.provision(environment).await;
        self.building.lock().await.remove(environment);
        result
    }

    async fn provision(&self, environment: &str) -> Result<AppState, String> {
        let (state, telemetry) = self.factory.build(environment).await?;
        self.telemetry.register_environment(
            environment,
            &telemetry,
            state.object_store.clone(),
            &self.factory.config.deployment_env,
        );
        let state = self.registry.insert(state);
        self.workers.ensure_started(&state);
        tracing::info!(
            environment_id = %environment,
            loaded = self.registry.len(),
            "Loaded environment runtime"
        );
        Ok(state)
    }
}

/// How often [`ClusterWorkers`](crate::cluster_lifecycle::ClusterWorkers)
/// looks for unloaded environments with pending background jobs.
pub const RECOVERY_INTERVAL: Duration = Duration::from_secs(60);
/// Environments confirmed and loaded at once during one recovery pass.
const RECOVERY_CONCURRENCY: usize = 4;
/// Upper bound on confirming and loading one environment. A metadata API call
/// alone may take 10s; an environment that runs out of time is retried on the
/// next pass instead of holding up the others.
const RECOVERY_ENVIRONMENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Resumes background jobs for environments this process has not loaded:
/// after a restart, or when another replica that served them goes away.
pub struct PendingJobRecovery {
    pool: sqlx::PgPool,
    directory: Arc<dyn EnvironmentDirectory>,
    provisioner: Arc<RuntimeProvisioner>,
    registry: Arc<EnvironmentRegistry>,
    environment_timeout: Duration,
    skipped: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl PendingJobRecovery {
    pub fn new(
        pool: sqlx::PgPool,
        directory: Arc<dyn EnvironmentDirectory>,
        provisioner: Arc<RuntimeProvisioner>,
        registry: Arc<EnvironmentRegistry>,
    ) -> Self {
        Self {
            pool,
            directory,
            provisioner,
            registry,
            environment_timeout: RECOVERY_ENVIRONMENT_TIMEOUT,
            skipped: Default::default(),
        }
    }

    /// Override the per-environment time limit (tests use a short one).
    pub fn with_environment_timeout(mut self, timeout: Duration) -> Self {
        self.environment_timeout = timeout;
        self
    }

    /// Load every confirmed environment with pending jobs, a few at a time.
    /// Directory outages, build failures and timeouts are retried on the next
    /// run; one slow environment never holds up the rest.
    pub async fn run(&self) {
        let pending = match crate::background_responses::environments_with_pending_jobs(&self.pool)
            .await
        {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(%error, "could not list environments with pending background jobs");
                return;
            }
        };
        let unloaded = pending
            .into_iter()
            .filter(|environment| self.registry.get(environment).is_none());
        stream::iter(unloaded)
            .for_each_concurrent(RECOVERY_CONCURRENCY, |environment| async move {
                if tokio::time::timeout(self.environment_timeout, self.recover(&environment))
                    .await
                    .is_err()
                {
                    tracing::warn!(environment_id = %environment, timeout = ?self.environment_timeout, "loading an environment with pending background jobs timed out; retrying on the next attempt");
                }
            })
            .await;
    }

    async fn recover(&self, environment: &str) {
        match self.directory.lookup(environment).await {
            Ok(Some(_)) => {
                if let Err(error) = self.provisioner.get_or_provision(environment).await {
                    tracing::warn!(environment_id = %environment, %error, "could not load environment with pending background jobs");
                }
            }
            Ok(None) => {
                let first = self
                    .skipped
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(environment.to_owned());
                if first {
                    tracing::warn!(environment_id = %environment, "not resuming background jobs for an environment this cluster does not serve");
                }
            }
            Err(error) => {
                tracing::warn!(environment_id = %environment, %error, "environment directory unavailable; background jobs resume on the next attempt");
            }
        }
    }
}
