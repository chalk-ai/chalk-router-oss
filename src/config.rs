//! Runtime configuration resolved from the environment.
//!
//! Backends for the usage ledger and mutable quota state are selected here.
//! Budget and rate-limit definitions follow the core store; only their shared
//! admission state is selected independently.

use std::time::Duration;

/// Usage-ledger backend (records per-request token usage).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageBackendKind {
    Disabled,
    /// Plain PostgreSQL ledger (`chalk_router.usage_events`, on-the-fly aggregation).
    Postgres,
    Clickhouse,
}

/// Shared mutable-state backend for budget and rate-limit enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaBackendKind {
    Disabled,
    /// In-process and volatile: local development and tests only.
    Memory,
    Postgres,
    /// Reserved until an atomic Redis implementation passes conformance.
    Redis,
}

/// Prompt-cache backend (exact-prefix completion cache; see
/// [`crate::prompt_cache`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromptCacheBackendKind {
    #[default]
    Disabled,
    /// In-process, bounded, LRU. Per-replica and lost on restart — fine for a
    /// single replica or local development, not a shared cache.
    Memory,
    /// Valkey (or any RESP-compatible Redis), shared across replicas.
    Valkey,
}

/// Deployment wiring for the prompt cache. What each *token* gets — whether it
/// is cached at all, and at what scope — is per-key policy
/// ([`crate::prompt_cache::CachePolicy`]); this is only the parts an operator
/// sets per rollout, plus the defaults an unset per-key field inherits.
#[derive(Debug, Clone)]
pub struct PromptCacheConfig {
    pub backend: PromptCacheBackendKind,
    /// `ROUTER_PROMPT_CACHE_URL`, required by the `valkey` backend.
    pub url: Option<String>,
    /// Valkey key prefix, so one Valkey can serve several deployments
    /// (`ROUTER_PROMPT_CACHE_NAMESPACE`).
    pub namespace: String,
    /// Default entry lifetime for keys that set no `ttl_secs` of their own.
    pub ttl: Duration,
    /// Ceiling on one stored entry (the response payload plus its bookkeeping;
    /// see `CachedCompletion::weight`). Also the hard cap a per-key
    /// `max_entry_bytes` is clamped to, so no single token can push oversized
    /// entries into a shared cache.
    pub max_entry_bytes: usize,
    /// `memory` backend only: entry-count bound.
    pub max_entries: usize,
    /// `memory` backend only: total byte budget.
    pub max_bytes: usize,
}

impl PromptCacheConfig {
    /// The no-cache configuration. Also the defaults every other field
    /// inherits, so a partially-configured deployment is still coherent.
    pub fn disabled() -> Self {
        Self {
            backend: PromptCacheBackendKind::Disabled,
            url: None,
            namespace: "chalk-router:prompt-cache".to_string(),
            // Long enough to be worth having across an eval run or a working
            // day of agent traffic; short enough that a model or prompt-template
            // change washes out of the cache without an operator flushing it.
            ttl: Duration::from_secs(3600),
            max_entry_bytes: 1 << 20,
            max_entries: 10_000,
            max_bytes: 256 << 20,
        }
    }

    pub fn from_env() -> Self {
        let defaults = Self::disabled();
        let backend = match env_nonempty("ROUTER_PROMPT_CACHE_BACKEND").as_deref() {
            None | Some("disabled") => PromptCacheBackendKind::Disabled,
            Some("memory") => PromptCacheBackendKind::Memory,
            // `redis` is accepted as an alias: the wire protocol is the same,
            // and a deployment pointed at a Redis instance should not have to
            // call it something else.
            Some("valkey") | Some("redis") => PromptCacheBackendKind::Valkey,
            Some(other) => {
                tracing::warn!(
                    value = other,
                    "unknown ROUTER_PROMPT_CACHE_BACKEND; defaulting to disabled"
                );
                PromptCacheBackendKind::Disabled
            }
        };
        Self {
            backend,
            url: env_nonempty("ROUTER_PROMPT_CACHE_URL"),
            namespace: env_nonempty("ROUTER_PROMPT_CACHE_NAMESPACE").unwrap_or(defaults.namespace),
            ttl: Duration::from_secs(
                env_parse("ROUTER_PROMPT_CACHE_TTL_SECS")
                    .filter(|secs: &u64| *secs > 0)
                    .unwrap_or(defaults.ttl.as_secs()),
            ),
            max_entry_bytes: env_parse("ROUTER_PROMPT_CACHE_MAX_ENTRY_BYTES")
                .filter(|b: &usize| *b > 0)
                .unwrap_or(defaults.max_entry_bytes),
            max_entries: env_parse("ROUTER_PROMPT_CACHE_MAX_ENTRIES")
                .filter(|n: &usize| *n > 0)
                .unwrap_or(defaults.max_entries),
            max_bytes: env_parse("ROUTER_PROMPT_CACHE_MAX_BYTES")
                .filter(|b: &usize| *b > 0)
                .unwrap_or(defaults.max_bytes),
        }
    }
}

/// Judge backend selected by deployment (which *kind* of judge runs). The judge
/// *model*, ladders, thresholds, defaults, and enable/shadow flags are
/// admin-editable and live in the DB `JudgePolicy`, not here — env holds only
/// deployment wiring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JudgeBackendKind {
    /// Remote lightweight LLM reached through the shared router backend (the
    /// judge model is a public model id resolved via the routing snapshot).
    Remote,
    /// Small model served over localhost (sidecar); no content leaves the pod.
    Local,
    /// Trained classifier head (not yet implemented; effectively disables judging).
    Classifier,
}

/// Deployment wiring for the model-as-judge feature. Everything an admin edits
/// lives in the DB policy; this is only the parts an operator sets per rollout.
/// `None` from [`JudgeConfig::from_env`] (no `ROUTER_JUDGE_BACKEND`) means the
/// feature is not deployed at all.
#[derive(Debug, Clone)]
pub struct JudgeConfig {
    pub backend: JudgeBackendKind,
    /// Local sidecar base URL (`ROUTER_JUDGE_BASE_URL`), used when `backend` is
    /// `Local`.
    pub base_url: Option<String>,
    /// Per-call judge deadline; on timeout the caller fails open.
    pub timeout: std::time::Duration,
    /// Max concurrent judge calls (bulkhead). Excess calls skip the judge
    /// (fail open) via `try_acquire` rather than queueing.
    pub max_concurrency: usize,
    /// Global kill switch (`ROUTER_JUDGE_KILL_SWITCH`), overrides the DB policy.
    pub kill_switch: bool,
    pub cache_capacity: usize,
    pub cache_ttl: std::time::Duration,
    /// Heuristic pre-filter (`ROUTER_JUDGE_HEURISTIC`, default on): sub-ms
    /// signals short-circuit obviously-easy/hard requests before any LLM call.
    pub heuristic_prefilter: bool,
    /// Session affinity (`ROUTER_JUDGE_SESSION_AFFINITY`, default on): pin the
    /// first-turn difficulty for a conversation so later turns neither re-judge
    /// nor flip the served model (hard heuristic signals still ratchet UP).
    pub session_affinity: bool,
    /// How often each replica re-reads the policy and re-probes the judge
    /// backend (`ROUTER_JUDGE_REVALIDATE_SECS`, default 60). Policy edits land
    /// on ONE replica's gRPC handler; every other replica only learns "the
    /// judge is now enabled/usable" from this loop — without it, a replica
    /// that booted while the policy was disabled would stay unconfigured (and
    /// judge-less) until restart.
    pub revalidate_interval: std::time::Duration,
}

impl JudgeConfig {
    /// `None` when `ROUTER_JUDGE_BACKEND` is unset/empty — the judge is not
    /// deployed and adds zero cost to the request path.
    pub fn from_env() -> Option<Self> {
        let backend = match env_nonempty("ROUTER_JUDGE_BACKEND")?.as_str() {
            "remote" => JudgeBackendKind::Remote,
            "local" => JudgeBackendKind::Local,
            "classifier" => JudgeBackendKind::Classifier,
            other => {
                tracing::warn!(
                    value = other,
                    "unknown ROUTER_JUDGE_BACKEND; judge disabled"
                );
                return None;
            }
        };
        Some(JudgeConfig {
            backend,
            base_url: env_nonempty("ROUTER_JUDGE_BASE_URL"),
            timeout: std::time::Duration::from_millis(
                env_parse("ROUTER_JUDGE_TIMEOUT_MS").unwrap_or(700),
            ),
            max_concurrency: env_parse("ROUTER_JUDGE_MAX_CONCURRENCY").unwrap_or(8),
            kill_switch: env_flag("ROUTER_JUDGE_KILL_SWITCH"),
            cache_capacity: env_parse("ROUTER_JUDGE_CACHE_CAPACITY").unwrap_or(10_000),
            cache_ttl: std::time::Duration::from_secs(
                // Long enough to pin a whole conversation (session affinity),
                // short enough that a rubric/ladder recalibration takes effect
                // within the hour.
                env_parse("ROUTER_JUDGE_CACHE_TTL_SECS").unwrap_or(1800),
            ),
            heuristic_prefilter: !env_off("ROUTER_JUDGE_HEURISTIC"),
            session_affinity: !env_off("ROUTER_JUDGE_SESSION_AFFINITY"),
            revalidate_interval: std::time::Duration::from_secs(
                env_parse("ROUTER_JUDGE_REVALIDATE_SECS")
                    .unwrap_or(60)
                    .max(1),
            ),
        })
    }
}

/// Whether a base URL points at the local host (loopback IP or `localhost`).
///
/// The `local` judge backend bypasses the per-key egress permission gate on
/// the promise that request content never leaves the pod — a promise only a
/// loopback address can keep. Anything else (a LAN IP, a cluster service, a
/// public host) must not be deployed as `local`; `main` refuses to start the
/// judge rather than silently shipping customer prompts past the gate.
pub fn is_loopback_base_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(v4) = host.parse::<std::net::Ipv4Addr>() {
        return v4.is_loopback();
    }
    // Url keeps IPv6 hosts bracketed (`[::1]`).
    if let Ok(v6) = host.trim_matches(['[', ']']).parse::<std::net::Ipv6Addr>() {
        return v6.is_loopback();
    }
    false
}

/// Fully-resolved router configuration.
#[derive(Clone)]
pub struct RouterConfig {
    /// Cluster-mode settings; `None` in environment mode. From
    /// `ROUTER_TENANCY_MODE=cluster` (see [`ClusterConfig`]).
    pub cluster: Option<ClusterConfig>,
    /// `CHALK_ENVIRONMENT_ID`. In cluster mode, the optional legacy environment
    /// that keeps its static admin key, default exporters and response locators.
    pub legacy_environment_id: Option<String>,
    /// The environment this deployment serves. Every store query is scoped by
    /// it (multi-tenant DB / security boundary); required when a database is
    /// configured. In cluster mode this is only the default bundle (the legacy
    /// environment, or [`CLUSTER_TEMPLATE_ENVIRONMENT_ID`]): authenticated
    /// requests select their own environment.
    pub environment_id: String,
    /// Default-pool database (core tables). From `ROUTER_DATABASE_URL`. When
    /// `None`, in-memory stores are used (local dev).
    pub database_url: Option<String>,
    pub usage_backend: UsageBackendKind,
    pub quota_backend: QuotaBackendKind,
    pub background_responses: crate::background_responses::PollConfig,
    /// Exact-prefix completion cache wiring (see [`PromptCacheConfig`]).
    pub prompt_cache: PromptCacheConfig,

    /// Core Postgres pool size (`ROUTER_DATABASE_MAX_CONNECTIONS`, default 20).
    pub database_max_connections: u32,
    /// Core Postgres pool acquire timeout (`ROUTER_DATABASE_ACQUIRE_TIMEOUT_MS`,
    /// default 5000). sqlx's own default is ~30s; a tighter timeout fails a
    /// request fast under pool exhaustion instead of piling up latency behind
    /// a slow-draining queue of connection waiters.
    pub database_acquire_timeout: std::time::Duration,
    /// Ping idle Postgres connections on acquisition after this interval
    /// (`ROUTER_DATABASE_HEALTH_CHECK_IDLE_MS`, default 1000). Zero restores
    /// unconditional checks. SQLx still validates every returned connection.
    pub database_health_check_idle: std::time::Duration,
    /// TCP/TLS connect timeout for every upstream request
    /// (`ROUTER_UPSTREAM_CONNECT_TIMEOUT_MS`, default 10000). Without it a
    /// provider that stops answering SYNs holds a request for the OS connect
    /// timeout (about two minutes) before the fallback walk can move on. Only
    /// the connect phase is bounded: a non-streaming generation may legitimately
    /// take minutes before its first byte, and the routing walk deadline bounds
    /// the whole attempt.
    pub upstream_connect_timeout: std::time::Duration,

    /// Stdout log formatter (`LOG_FORMAT`).
    pub log_format: LogFormat,
    /// `deployment.environment` OTel resource attribute (`ENV`, default
    /// `"local"`). Distinct from `environment_id`: this is a free-text
    /// deployment label for telemetry, not the tenant-isolation boundary.
    pub deployment_env: String,
    /// Datadog `env` facet stamped on JSON log lines (`DD_ENV`).
    pub dd_env: Option<String>,

    /// Upload/attachment object store URI (`ROUTER_OBJECT_STORE_URI`).
    /// `None` disables file uploads.
    pub object_store_uri: Option<String>,
    /// `aud` pinned on federated Chalk JWTs (`CHALK_JWT_AUDIENCE`).
    pub chalk_jwt_audience: Option<String>,
    /// Explicit opt-in to recording request/response bodies on traces.
    pub trace_log_content: bool,
    /// Comma-separated federated-JWT issuer list (`CHALK_JWT_ISSUERS`); see
    /// [`crate::parse_chalk_jwt_issuers`].
    pub chalk_jwt_issuers: Option<String>,
    /// Environment owner's team (`CHALK_TEAM_ID`), required for environment-mode
    /// JWT administration. Cluster mode proves ownership through its directory.
    pub federated_admin_team_id: Option<String>,
    /// Static admin credential, alternative to a federated JWT
    /// (`ROUTER_ADMIN_KEY`; empty/unset => `None`).
    pub admin_key: Option<String>,
    /// Local-development opt-in for proxy requests without credentials
    /// (`ROUTER_DANGEROUSLY_PERMIT_UNAUTHENTICATED`; default false).
    /// Admin routes still require authentication.
    pub dangerously_permit_unauthenticated: bool,
    /// Accept API keys/JWTs scoped to a different `environment_id`
    /// (`ROUTER_ACCEPT_CROSS_ENVIRONMENT_TOKENS`).
    pub accept_cross_environment_tokens: bool,
    /// Validated listener (`ROUTER_BIND_ADDRESS`, default loopback; `PORT`, default 4001).
    pub listen_addr: std::net::SocketAddr,
}

impl RouterConfig {
    pub fn from_env() -> Self {
        let database_url = env_nonempty("ROUTER_DATABASE_URL");
        let tenancy = tenancy_mode().unwrap_or_else(|error| panic!("{error}"));
        let legacy_environment_id = env_nonempty("CHALK_ENVIRONMENT_ID");

        // environment_id is the tenancy/security boundary. Environment mode
        // requires it whenever a database is configured (fail fast); the
        // in-memory dev path defaults it. Cluster mode selects environments per
        // request and only keeps an optional legacy environment.
        let environment_id = match (&legacy_environment_id, tenancy) {
            (Some(env), _) => env.clone(),
            (None, TenancyMode::Cluster) => CLUSTER_TEMPLATE_ENVIRONMENT_ID.to_string(),
            (None, TenancyMode::Environment) if database_url.is_some() => {
                panic!("CHALK_ENVIRONMENT_ID must be set (non-empty) when a database is configured — it scopes every query and is a tenant-isolation boundary")
            }
            (None, TenancyMode::Environment) => "develop".to_string(),
        };

        let cluster = (tenancy == TenancyMode::Cluster).then(|| {
            assert!(
                env_nonempty("ROUTER_ADMIN_KEY").is_none() || legacy_environment_id.is_some(),
                "cluster ROUTER_ADMIN_KEY requires an explicit CHALK_ENVIRONMENT_ID"
            );
            assert!(
                !env_flag("ROUTER_DANGEROUSLY_PERMIT_UNAUTHENTICATED"),
                "cluster routers require authenticated credentials"
            );
            ClusterConfig {
                metadata_api_endpoint: env_nonempty("ROUTER_METADATA_API_ENDPOINT")
                    .expect("cluster mode requires ROUTER_METADATA_API_ENDPOINT"),
                metadata_api_token: env_nonempty("ROUTER_METADATA_API_TOKEN")
                    .expect("cluster mode requires ROUTER_METADATA_API_TOKEN"),
                kube_cluster_id: env_nonempty("ROUTER_KUBE_CLUSTER_ID"),
            }
        });

        let dangerously_permit_unauthenticated =
            env_flag("ROUTER_DANGEROUSLY_PERMIT_UNAUTHENTICATED");
        let bind_address: std::net::IpAddr = std::env::var("ROUTER_BIND_ADDRESS")
            .unwrap_or_else(|_| "127.0.0.1".to_string())
            .parse()
            .expect("ROUTER_BIND_ADDRESS must be an IPv4 or IPv6 address");
        let port: u16 = std::env::var("PORT")
            .unwrap_or_else(|_| "4001".to_string())
            .parse()
            .expect("PORT must be an integer from 0 to 65535");
        assert!(
            !dangerously_permit_unauthenticated || bind_address.is_loopback()
                || env_flag("ROUTER_DANGEROUSLY_PERMIT_UNAUTHENTICATED_NON_LOOPBACK"),
            "anonymous access on a non-loopback address requires ROUTER_DANGEROUSLY_PERMIT_UNAUTHENTICATED_NON_LOOPBACK=true"
        );

        RouterConfig {
            cluster,
            legacy_environment_id,
            environment_id,
            database_url,
            usage_backend: parse_usage(env_nonempty("ROUTER_USAGE_BACKEND")),
            quota_backend: parse_quota(env_nonempty("ROUTER_QUOTA_BACKEND")),
            background_responses: crate::background_responses::PollConfig::new(
                env_parse("ROUTER_BACKGROUND_RESPONSES_POLL_INTERVAL_SECS").unwrap_or(30),
                env_parse("ROUTER_BACKGROUND_RESPONSES_POLL_BATCH_SIZE").unwrap_or(100),
                env_parse("ROUTER_BACKGROUND_RESPONSES_POLL_CONCURRENCY").unwrap_or(4),
            )
            .with_retention_days(
                env_parse("ROUTER_BACKGROUND_RESPONSES_RETENTION_DAYS").unwrap_or(30),
            ),
            prompt_cache: PromptCacheConfig::from_env(),
            database_max_connections: env_parse("ROUTER_DATABASE_MAX_CONNECTIONS").unwrap_or(20),
            database_acquire_timeout: std::time::Duration::from_millis(
                env_parse("ROUTER_DATABASE_ACQUIRE_TIMEOUT_MS").unwrap_or(5000),
            ),
            database_health_check_idle: std::time::Duration::from_millis(
                env_parse("ROUTER_DATABASE_HEALTH_CHECK_IDLE_MS").unwrap_or(1000),
            ),
            upstream_connect_timeout: std::time::Duration::from_millis(
                env_parse("ROUTER_UPSTREAM_CONNECT_TIMEOUT_MS").unwrap_or(10_000),
            ),

            log_format: LogFormat::from_env(),
            // Matches the pre-consolidation read exactly: only an unset `ENV`
            // (not an explicitly empty one) falls back to "local".
            deployment_env: std::env::var("ENV").unwrap_or_else(|_| "local".to_string()),
            dd_env: env_nonempty("DD_ENV"),

            // Raw (unfiltered) reads below intentionally preserve each site's
            // pre-consolidation behavior for an explicitly-empty value, rather
            // than switching to `env_nonempty`'s empty-string-means-unset rule.
            object_store_uri: std::env::var("ROUTER_OBJECT_STORE_URI").ok(),
            trace_log_content: parse_trace_log_content(
                std::env::var("ROUTER_TRACE_LOG_CONTENT").ok().as_deref(),
            )
            .unwrap_or_else(|error| panic!("{error}")),
            chalk_jwt_audience: env_nonempty("CHALK_JWT_AUDIENCE"),
            chalk_jwt_issuers: std::env::var("CHALK_JWT_ISSUERS").ok(),
            federated_admin_team_id: env_nonempty("CHALK_TEAM_ID"),
            admin_key: env_nonempty("ROUTER_ADMIN_KEY"),
            dangerously_permit_unauthenticated,
            accept_cross_environment_tokens: env_bool_loose(
                "ROUTER_ACCEPT_CROSS_ENVIRONMENT_TOKENS",
            ),
            listen_addr: std::net::SocketAddr::new(bind_address, port),
        }
    }

    /// Fail startup rather than accept JWTs intended for another audience or
    /// infer ownership from an untrusted request. Static-key-only installs need
    /// no federation configuration.
    pub fn federated_issuer_configs(
        &self,
    ) -> Result<Vec<chalk_federated_auth::IssuerConfig>, String> {
        let audience = self
            .chalk_jwt_audience
            .as_deref()
            .filter(|v| !v.trim().is_empty());
        let issuers = self
            .chalk_jwt_issuers
            .as_deref()
            .map(|spec| crate::parse_chalk_jwt_issuers(spec, audience))
            .unwrap_or_default();
        if !issuers.is_empty() {
            if audience.is_none() {
                return Err(
                    "CHALK_JWT_AUDIENCE is required when CHALK_JWT_ISSUERS is configured".into(),
                );
            }
            if self.cluster.is_none()
                && self
                    .federated_admin_team_id
                    .as_deref()
                    .is_none_or(|v| v.trim().is_empty())
            {
                return Err(
                    "CHALK_TEAM_ID is required for environment-mode JWT administration".into(),
                );
            }
        }
        Ok(issuers)
    }
}

/// Whether this process serves one configured environment or loads cluster
/// environments on demand. From `ROUTER_TENANCY_MODE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TenancyMode {
    /// One environment (`CHALK_ENVIRONMENT_ID`); never contacts the metadata API.
    #[default]
    Environment,
    /// Environments are confirmed with the metadata API and loaded on first use.
    Cluster,
}

/// Default bundle ID for a cluster router without a legacy environment. It is
/// never served: authenticated requests always select a confirmed environment.
pub const CLUSTER_TEMPLATE_ENVIRONMENT_ID: &str = "chalk-router-cluster-template";

/// Cluster-mode access to the Chalk metadata API, which confirms environments
/// and their owning teams.
#[derive(Clone)]
pub struct ClusterConfig {
    /// gRPC endpoint of the metadata API server (`ROUTER_METADATA_API_ENDPOINT`).
    pub metadata_api_endpoint: String,
    /// Long-lived exchange token, traded for short-lived working tokens
    /// (`ROUTER_METADATA_API_TOKEN`). Delivered through the serving secret.
    pub metadata_api_token: String,
    /// When set, only environments on this kube cluster are served
    /// (`ROUTER_KUBE_CLUSTER_ID`).
    pub kube_cluster_id: Option<String>,
}

pub fn tenancy_mode() -> Result<TenancyMode, String> {
    match env_nonempty("ROUTER_TENANCY_MODE").as_deref() {
        None | Some("environment") => Ok(TenancyMode::Environment),
        Some("cluster") => Ok(TenancyMode::Cluster),
        Some(other) => Err(format!(
            "ROUTER_TENANCY_MODE must be `environment` or `cluster`, not `{other}`"
        )),
    }
}

/// The `migrate` subcommand's configuration tripwire. No migration reads
/// `CHALK_ENVIRONMENT_ID`, but an environment-mode migrate Job missing it almost
/// certainly means the Deployment about to roll is missing it too, and failing
/// the Job is cheaper than failing the rollout. Cluster mode has no
/// deployment-wide environment and needs no metadata access to migrate.
pub fn check_migrate_environment() -> Result<(), String> {
    match tenancy_mode()? {
        TenancyMode::Cluster => Ok(()),
        TenancyMode::Environment if env_nonempty("CHALK_ENVIRONMENT_ID").is_some() => Ok(()),
        TenancyMode::Environment => Err("set CHALK_ENVIRONMENT_ID (no migration reads it; the migrate Job must carry the same configuration as the Deployment it precedes)".into()),
    }
}

/// Stdout log output format, selected by the `LOG_FORMAT` env var. Defaults to
/// Datadog-compatible JSON; `pretty` gives a human-readable format for local dev.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    #[default]
    Json,
    Pretty,
}

impl LogFormat {
    /// Resolve from `LOG_FORMAT` (case-insensitive). Anything other than `pretty`
    /// — including unset — is JSON, so production defaults to Datadog-facetable logs.
    pub fn from_env() -> Self {
        match env_nonempty("LOG_FORMAT").as_deref() {
            Some(v) if v.eq_ignore_ascii_case("pretty") => LogFormat::Pretty,
            _ => LogFormat::Json,
        }
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    env_nonempty(key).and_then(|v| v.parse().ok())
}

/// True when the variable is an explicit truthy flag (`1`/`true`).
fn env_flag(key: &str) -> bool {
    matches!(
        env_nonempty(key).as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("True")
    )
}

/// True when the variable is set to `"1"` or `"true"` (case-insensitively).
/// Auth settings accept arbitrary casing, unlike [`env_flag`]'s literal matches.
fn env_bool_loose(key: &str) -> bool {
    std::env::var(key)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// True when the variable explicitly disables a default-on feature
/// (`0`/`false`/`off`).
fn env_off(key: &str) -> bool {
    matches!(
        env_nonempty(key).as_deref(),
        Some("0") | Some("false") | Some("off") | Some("FALSE") | Some("OFF")
    )
}

fn parse_usage(v: Option<String>) -> UsageBackendKind {
    match v.as_deref() {
        None | Some("disabled") => UsageBackendKind::Disabled,
        Some("postgres") | Some("plain_postgres") => UsageBackendKind::Postgres,
        Some("clickhouse") => UsageBackendKind::Clickhouse,
        Some(other) => {
            tracing::warn!(
                value = other,
                "unknown ROUTER_USAGE_BACKEND; defaulting to disabled"
            );
            UsageBackendKind::Disabled
        }
    }
}

#[cfg(test)]
mod loopback_tests {
    use super::is_loopback_base_url;

    #[test]
    fn loopback_urls_accepted() {
        assert!(is_loopback_base_url("http://localhost:8080/v1"));
        assert!(is_loopback_base_url("http://LOCALHOST:8080"));
        assert!(is_loopback_base_url("http://127.0.0.1:9000"));
        assert!(is_loopback_base_url("http://127.1.2.3"));
        assert!(is_loopback_base_url("http://[::1]:8080/v1"));
    }

    #[test]
    fn non_loopback_urls_rejected() {
        assert!(!is_loopback_base_url("https://judge.example.com/v1"));
        assert!(!is_loopback_base_url("http://10.0.0.5:8080"));
        assert!(!is_loopback_base_url(
            "http://judge-sidecar.default.svc.cluster.local"
        ));
        assert!(!is_loopback_base_url("http://[fd00::1]:8080"));
        assert!(!is_loopback_base_url("not a url"));
    }
}

fn parse_quota(v: Option<String>) -> QuotaBackendKind {
    match v.as_deref() {
        None | Some("disabled") => QuotaBackendKind::Disabled,
        Some("postgres") | Some("plain_postgres") => QuotaBackendKind::Postgres,
        Some("memory") => QuotaBackendKind::Memory,
        Some("redis") => QuotaBackendKind::Redis,
        Some(other) => {
            tracing::warn!(
                value = other,
                "unknown ROUTER_QUOTA_BACKEND; defaulting to disabled"
            );
            QuotaBackendKind::Disabled
        }
    }
}

/// Invalid privacy settings are configuration errors, never consent to capture.
pub fn parse_trace_log_content(value: Option<&str>) -> Result<bool, &'static str> {
    match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        None => Ok(false),
        Some("true" | "1" | "on" | "yes") => Ok(true),
        Some("false" | "0" | "off" | "no") => Ok(false),
        _ => Err("ROUTER_TRACE_LOG_CONTENT must be true/false, 1/0, on/off, or yes/no"),
    }
}

#[cfg(test)]
mod trace_privacy_config_tests {
    #[test]
    fn content_capture_requires_valid_explicit_consent() {
        use super::parse_trace_log_content;
        assert_eq!(parse_trace_log_content(None), Ok(false));
        for value in ["false", "0", "off", "no", "FALSE"] {
            assert_eq!(parse_trace_log_content(Some(value)), Ok(false));
        }
        for value in ["true", "1", "on", "yes", " TRUE "] {
            assert_eq!(parse_trace_log_content(Some(value)), Ok(true));
        }
        for value in ["", " ", "flase", "enabled", "2"] {
            assert!(parse_trace_log_content(Some(value)).is_err());
        }
    }
}
