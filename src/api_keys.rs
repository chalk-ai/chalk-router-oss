use async_trait::async_trait;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

use crate::budget::ApiKeyBudgetInput;
use crate::hashing::{key_prefix, KeyHasher};
use crate::prompt_cache::CachePolicy;
use crate::ratelimit::{
    api_key_rate_limit_policies, ApiKeyRateLimitInput, RATE_LIMIT_IDENTITY_CONFLICT,
};
use crate::service::pagination::StablePageCursor;

/// One connection a key may reach, with its own optional model allowlist.
/// Pairs a connection with the models permitted on it — a flat connection
/// list plus a shared `models` axis on [`ApiKeyPermission`] can't express
/// that pairing; each would be checked independently, so "connection A with
/// model x, connection B with model y" would also incorrectly permit "A with
/// y" and "B with x".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiKeyConnectionScope {
    pub connection_id: String,
    /// Empty means any model on this connection.
    #[serde(default)]
    pub models: Vec<String>,
}

/// What an API key may call. With no restriction set the key may call any
/// model. Otherwise it may call the union of:
///
/// * the `chalk/*` model routes in [`Self::route_ids`], each with everything
///   that route dispatches to, on any connection; and
/// * direct model ids allowed by [`Self::connection_scopes`] (and, for keys
///   written before connection scopes existed, the legacy `provider`,
///   `connection_id` and `models` fields).
///
/// Connection scopes and legacy fields never grant a `chalk/*` route.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiKeyPermission {
    /// Legacy: restrict direct model ids to one provider kind. Read and
    /// enforced for keys that already carry it; the admin API never writes it
    /// and presents it folded into `connection_scopes`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Legacy: restrict direct model ids to one connection by immutable id.
    /// Read-only like `provider`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    /// Legacy: restrict direct model ids to these public ids. Read-only like
    /// `provider`; any `chalk/*` id here is ignored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    /// Model-as-judge opt-in, tri-state: `None` inherits the judge policy's
    /// `default_opt_in`; `Some(true)`/`Some(false)` override it per key. Only an
    /// opted-in key ever has its requests downgraded (see `crate::judge`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgradable: Option<bool>,
    /// Per-key downgrade floor as a public model id: the judge never serves this
    /// key anything cheaper than the floor. `None` inherits the policy's
    /// `default_floor`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_floor: Option<String>,
    /// Exact-prefix completion caching for this key (see
    /// [`crate::prompt_cache`]). `None` — the default — means the key is never
    /// served from cache and never populates one.
    ///
    /// Deliberately opt-in per key rather than a deployment default: the policy
    /// also chooses how far a cached answer travels
    /// ([`CacheScope`](crate::prompt_cache::CacheScope)), and a request whose
    /// answer may be handed to another token is a decision to make per key, not
    /// one to inherit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CachePolicy>,
    /// Direct model ids this key may call, per connection. See
    /// [`ApiKeyConnectionScope`] for why connection and models are paired.
    /// AND-ed with any legacy field the key still carries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_scopes: Option<Vec<ApiKeyConnectionScope>>,
    /// Model routes this key may call, by immutable route id, so deleting and
    /// recreating a route under the same public id does not carry its grants
    /// over. A grant covers the route's default target, judge classifier and
    /// outcomes, and its own fallbacks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_ids: Option<Vec<String>>,
    /// Set when the stored permission could not be read. The key is denied
    /// every model and connection until an operator replaces the permission.
    /// Never serialized, so the stored value is left as written.
    #[serde(skip)]
    pub unreadable: bool,
}

// Trace sampling has no per-key permission field. A rate for one key is a rule
// with `key_id` set on the environment's sampling policy
// ([`crate::sampling_policy`]), so there is one ordered list to read and no
// precedence between two mechanisms to reason about.

/// Whether `model` names a `chalk/*` model route rather than a direct model.
pub fn is_model_route(model: &str) -> bool {
    model.starts_with(crate::provider_registry::ROUTE_RESERVED_PREFIX)
}

/// The `/models` spelling of a bare model id on the connection that owns
/// the provider's canonical prefix: `gpt-6-luna` on the `openai/` connection
/// is `openai/gpt-6-luna`. `None` for prefixed ids, for providers without a
/// canonical prefix, and when the resolved connection owns a different prefix
/// (there the prefixed spelling would not reach this upstream model).
fn canonical_alias(
    resolution: &crate::backend::RouteResolution,
    public_model_id: &str,
) -> Option<String> {
    if public_model_id.is_empty() || public_model_id.contains('/') {
        return None;
    }
    let canonical = crate::provider_registry::canonical_prefix(&resolution.provider_kind)?;
    (resolution.connection_prefix.as_deref() == Some(canonical))
        .then(|| format!("{canonical}{public_model_id}"))
}

impl ApiKeyPermission {
    /// A permission that denies everything, standing in for a stored value
    /// that could not be read.
    pub fn unreadable() -> Self {
        Self {
            unreadable: true,
            ..Self::default()
        }
    }

    /// Read a stored permission. A value that does not parse denies the key
    /// rather than falling back to the unrestricted default. Unknown permission
    /// or connection-scope fields also fail parsing to avoid dropping restrictions.
    pub fn from_stored(key_prefix: &str, value: serde_json::Value) -> Self {
        serde_json::from_value(value).unwrap_or_else(|error| {
            tracing::error!(
                key_prefix,
                %error,
                "API key permission is unreadable; denying the key until it is replaced"
            );
            Self::unreadable()
        })
    }

    /// This key's active caching policy, or `None` when the key has none or has
    /// switched it off. The one place the request path asks "may I cache this?",
    /// so `enabled: false` and "never configured" cannot diverge at call sites.
    pub fn cache_policy(&self) -> Option<&CachePolicy> {
        self.cache.as_ref().filter(|policy| policy.enabled)
    }

    /// Whether this key carries any restriction. Unrestricted keys skip
    /// enforcement entirely, so callers can avoid resolving the route just to
    /// check an empty allowlist.
    pub fn is_restricted(&self) -> bool {
        self.unreadable || self.route_ids.is_some() || self.restricts_direct_models()
    }

    /// Whether the key carries a legacy field that the admin API no longer
    /// writes.
    pub fn has_legacy_fields(&self) -> bool {
        self.provider.is_some() || self.connection_id.is_some() || self.models.is_some()
    }

    /// Whether any field grants direct model ids. A restricted key without one
    /// may call only its model routes.
    fn restricts_direct_models(&self) -> bool {
        self.connection_scopes.is_some() || self.has_legacy_fields()
    }

    /// Whether [`Self::without_legacy_fields`] would change this permission.
    pub fn has_legacy_grants(&self) -> bool {
        self.has_legacy_fields()
            || self
                .connection_scopes
                .iter()
                .flatten()
                .any(|scope| scope.models.iter().any(|model| is_model_route(model)))
    }

    /// This permission as the admin API presents it: legacy fields restated
    /// as connection scopes, and `chalk/*` ids dropped from scope model lists
    /// (they never granted anything). It allows the same direct models on
    /// `connections`, given as `(id, provider kind)`, so saving it back does
    /// not change what the key may call there.
    ///
    /// A legacy `provider` grant also reaches connections of that kind created
    /// later; once restated, it covers only those in `connections`.
    pub fn without_legacy_fields(&self, connections: &[(String, String)]) -> Self {
        if !self.has_legacy_grants() {
            return self.clone();
        }
        let direct = |models: &[String]| -> Vec<String> {
            models
                .iter()
                .filter(|model| !is_model_route(model))
                .cloned()
                .collect()
        };
        // Scoped connections, then (for legacy fields, which may span every
        // connection) the legacy connection and every known connection.
        let scoped = self
            .connection_scopes
            .iter()
            .flatten()
            .map(|scope| scope.connection_id.as_str());
        let legacy = self
            .has_legacy_fields()
            .then(|| {
                self.connection_id
                    .as_deref()
                    .into_iter()
                    .chain(connections.iter().map(|(id, _)| id.as_str()))
            })
            .into_iter()
            .flatten();
        let mut candidates: Vec<&str> = Vec::new();
        for id in scoped.chain(legacy) {
            if !candidates.contains(&id) {
                candidates.push(id);
            }
        }
        let mut scopes = Vec::new();
        for id in candidates {
            if let Some(provider) = &self.provider {
                let kind = connections
                    .iter()
                    .find(|(connection, _)| connection == id)
                    .map(|(_, kind)| kind);
                if kind != Some(provider) {
                    continue;
                }
            }
            if self.connection_id.as_deref().is_some_and(|only| only != id) {
                continue;
            }
            // `None` allows any model on the connection.
            let mut allowed: Option<Vec<String>> = None;
            if let Some(existing) = &self.connection_scopes {
                let Some(scope) = existing.iter().find(|scope| scope.connection_id == id) else {
                    continue;
                };
                if !scope.models.is_empty() {
                    allowed = Some(direct(&scope.models));
                }
            }
            if let Some(models) = &self.models {
                let listed = direct(models);
                allowed = Some(match allowed {
                    None => listed,
                    Some(allowed) => allowed
                        .into_iter()
                        .filter(|model| listed.contains(model))
                        .collect(),
                });
            }
            match allowed {
                Some(models) if models.is_empty() => {}
                allowed => scopes.push(ApiKeyConnectionScope {
                    connection_id: id.to_string(),
                    models: allowed.unwrap_or_default(),
                }),
            }
        }
        Self {
            provider: None,
            connection_id: None,
            models: None,
            connection_scopes: Some(scopes),
            ..self.clone()
        }
    }

    /// Whether everything the model route `model_route_id` dispatches to is
    /// granted: the key is unrestricted, or the route is in `route_ids`.
    pub fn grants_model_route(&self, model_route_id: Option<&str>) -> bool {
        if self.unreadable {
            return false;
        }
        if !self.is_restricted() {
            return true;
        }
        match (&self.route_ids, model_route_id) {
            (Some(ids), Some(id)) => ids.iter().any(|granted| granted == id),
            _ => false,
        }
    }

    /// Enforce the key against one resolved attempt, authorized under
    /// `public_model_id`: the requested model on the primary, or a fallback's
    /// own public id. A `chalk/*` id is allowed only by its route grant, using
    /// the route id the resolution carries; anything else by
    /// [`Self::allows`].
    ///
    /// `/models` lists a connection's models under the prefix it owns, so a
    /// scope names `openai/gpt-6-luna`; a client built against the provider's
    /// SDK sends the bare `gpt-6-luna`, which the router routes to the
    /// provider's implicit connection. When that connection owns the
    /// provider's canonical prefix, the two spellings reach the same connection
    /// and upstream model, so the listed spelling is accepted for the bare
    /// request. A connection with a custom prefix and a listed bare id granting
    /// the prefixed form are still exact matches.
    pub fn allows_route(
        &self,
        resolution: &crate::backend::RouteResolution,
        public_model_id: &str,
    ) -> Result<(), ApiKeyError> {
        self.allows_model(
            resolution,
            public_model_id,
            resolution.model_route_id.as_deref(),
        )
    }

    /// [`Self::allows_route`] with the route id given explicitly, for an
    /// attempt authorized under a route it did not resolve through: a judge
    /// selection or a route-owned fallback, which resolve to concrete targets.
    pub fn allows_model(
        &self,
        resolution: &crate::backend::RouteResolution,
        public_model_id: &str,
        model_route_id: Option<&str>,
    ) -> Result<(), ApiKeyError> {
        if self.unreadable {
            return Err(ApiKeyError::PermissionUnreadable);
        }
        if is_model_route(public_model_id) {
            return if self.grants_model_route(model_route_id) {
                Ok(())
            } else {
                Err(ApiKeyError::ModelDenied {
                    model: public_model_id.to_string(),
                })
            };
        }
        let provider_kind = resolution.provider_kind.as_str();
        let connection_id = resolution.connection_id.as_deref();
        match self.allows(provider_kind, connection_id, public_model_id) {
            Err(denied @ ApiKeyError::ModelDenied { .. }) => {
                match canonical_alias(resolution, public_model_id) {
                    Some(alias) if self.allows(provider_kind, connection_id, &alias).is_ok() => {
                        Ok(())
                    }
                    _ => Err(denied),
                }
            }
            other => other,
        }
    }

    /// Enforce the key's direct-model grants against one attempt.
    /// `connection_id` is the immutable id of the connection the attempt
    /// resolved through; `public_model_id` is the public id it is authorized
    /// under, never prefix-stripped. A `chalk/*` id is never granted here.
    ///
    /// The fields are AND-ed; `None` on any of them means no restriction from
    /// that field:
    ///
    /// * `connection_scopes` must contain an entry for the connection whose
    ///   `models` is empty or contains the public id.
    /// * Legacy `provider` must equal the real provider **kind**, never a
    ///   connection name; legacy `connection_id` must equal the connection's
    ///   id (a route that resolved without a stored connection is denied); and
    ///   legacy `models` must contain the public id.
    ///
    /// A restricted key with none of these fields may call no direct model.
    pub fn allows(
        &self,
        provider_kind: &str,
        connection_id: Option<&str>,
        public_model_id: &str,
    ) -> Result<(), ApiKeyError> {
        if self.unreadable {
            return Err(ApiKeyError::PermissionUnreadable);
        }
        if !self.is_restricted() {
            return Ok(());
        }
        let model_denied = || ApiKeyError::ModelDenied {
            model: public_model_id.to_string(),
        };
        if !self.restricts_direct_models() || is_model_route(public_model_id) {
            return Err(model_denied());
        }
        self.allows_connection(provider_kind, connection_id)?;
        if let Some(ref models) = self.models {
            if !models.iter().any(|m| m == public_model_id) {
                return Err(model_denied());
            }
        }
        if let Some(ref scopes) = self.connection_scopes {
            let scope =
                connection_id.and_then(|id| scopes.iter().find(|scope| scope.connection_id == id));
            if scope.is_some_and(|scope| {
                !scope.models.is_empty() && !scope.models.iter().any(|m| m == public_model_id)
            }) {
                return Err(model_denied());
            }
        }
        Ok(())
    }

    /// The connection half of [`Self::allows`]: provider kind and connection
    /// identity, without models.
    fn allows_connection(
        &self,
        provider_kind: &str,
        connection_id: Option<&str>,
    ) -> Result<(), ApiKeyError> {
        if let Some(ref allowed) = self.provider {
            if allowed != provider_kind {
                return Err(ApiKeyError::ProviderDenied {
                    provider: provider_kind.to_string(),
                });
            }
        }
        if let Some(ref allowed_id) = self.connection_id {
            if connection_id != Some(allowed_id.as_str()) {
                return Err(ApiKeyError::ConnectionDenied);
            }
        }
        if let Some(ref scopes) = self.connection_scopes {
            if !connection_id.is_some_and(|id| scopes.iter().any(|s| s.connection_id == id)) {
                return Err(ApiKeyError::ConnectionDenied);
            }
        }
        Ok(())
    }

    /// Data-governance gate for a call that sends request content to a
    /// connection outside the attempt itself: a route judge's classifier, or
    /// retrieving a stored response by id.
    ///
    /// A call made for a granted model route (`model_route_id`) is part of
    /// that route and allowed. Otherwise only the connection is checked —
    /// provider kind and connection identity, never model lists, which govern
    /// what the key may call rather than which providers may see its content.
    pub fn allows_egress(
        &self,
        model_route_id: Option<&str>,
        provider_kind: &str,
        connection_id: Option<&str>,
    ) -> Result<(), ApiKeyError> {
        if self.unreadable {
            return Err(ApiKeyError::PermissionUnreadable);
        }
        if self.grants_model_route(model_route_id) {
            return Ok(());
        }
        if !self.restricts_direct_models() {
            return Err(ApiKeyError::ConnectionDenied);
        }
        self.allows_connection(provider_kind, connection_id)
    }
}

/// Point-in-time usage counters. The usage backend constructs these on demand
/// by querying its persistent ledger directly (see
/// [`UsageTracker::snapshots`](crate::usage_tracker::UsageTracker::snapshots)),
/// so a call's usage is invisible here until its write to that ledger commits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct UsageSnapshot {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub request_count: u64,
}

/// A stored API key. The raw secret is never held here — only its
/// `key_hash` (HMAC-SHA256, the primary key) and a non-secret `key_prefix`
/// for display. `key_hash` is the identifier used for admin operations and
/// usage attribution.
#[derive(Debug)]
pub struct ApiKey {
    pub key_hash: String,
    pub key_prefix: String,
    pub description: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub permission: ApiKeyPermission,
    /// Immutable machine-readable metadata set when the key is created.
    pub labels: HashMap<String, String>,
    pub usage_pool_id: Option<String>,
    /// Transient: budgets live in `chalk_router.budget_limits` via the budget
    /// backend and are not persisted on the key. Always `None` when loaded.
    pub daily_token_budget: Option<u64>,
}

impl Serialize for ApiKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("ApiKey", 8)?;
        s.serialize_field("key_hash", &self.key_hash)?;
        s.serialize_field("key_prefix", &self.key_prefix)?;
        s.serialize_field("description", &self.description)?;
        s.serialize_field("created_at", &self.created_at)?;
        s.serialize_field("permission", &self.permission)?;
        s.serialize_field("labels", &self.labels)?;
        // Stored key metadata does not own usage. Admin list handlers replace
        // this compatibility default with a read-time usage-backend snapshot.
        s.serialize_field("usage", &UsageSnapshot::default())?;
        s.serialize_field("usage_pool_id", &self.usage_pool_id)?;
        s.end()
    }
}

/// Returned by [`ApiKeyStore::create`]. The raw secret is exposed exactly
/// once, here, and never persisted or returned again.
#[derive(Debug)]
pub struct CreatedApiKey {
    pub raw_key: String,
    pub api_key: Arc<ApiKey>,
}

#[derive(Debug)]
pub enum ApiKeyError {
    NotFound,
    ProviderDenied {
        provider: String,
    },
    /// The key is scoped (via `connection_id`) to a specific connection, and the
    /// request resolved through a different one (or through no stored connection).
    ConnectionDenied,
    ModelDenied {
        model: String,
    },
    /// The stored permission could not be read, so the key is denied.
    PermissionUnreadable,
}

impl std::fmt::Display for ApiKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiKeyError::NotFound => write!(f, "Invalid API key"),
            ApiKeyError::ProviderDenied { provider } => {
                write!(f, "API key does not have access to provider '{provider}'")
            }
            ApiKeyError::ConnectionDenied => {
                write!(
                    f,
                    "API key is not authorized for the resolved provider connection"
                )
            }
            ApiKeyError::ModelDenied { model } => {
                write!(f, "API key does not have access to model '{model}'")
            }
            ApiKeyError::PermissionUnreadable => {
                write!(
                    f,
                    "API key permission could not be read; an administrator must replace it"
                )
            }
        }
    }
}

/// Persistence failure from an [`ApiKeyStore`] write.
#[derive(Debug)]
pub enum ApiKeyStoreError {
    /// `usage_pool_id` references a pool that does not exist (FK violation, 23503).
    InvalidUsagePool,
    /// An inline rate-limit selector references a provider connection that does
    /// not exist in this environment.
    InvalidRateLimitConnection,
    /// Inline definitions contain or race with the same canonical identity.
    DuplicateRateLimit,
    /// Any other persistence failure.
    Internal(String),
}

impl std::fmt::Display for ApiKeyStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiKeyStoreError::InvalidUsagePool => write!(f, "usage_pool_id does not exist"),
            ApiKeyStoreError::InvalidRateLimitConnection => {
                write!(f, "rate limit connection_id does not exist")
            }
            ApiKeyStoreError::DuplicateRateLimit => write!(f, "{RATE_LIMIT_IDENTITY_CONFLICT}"),
            ApiKeyStoreError::Internal(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ApiKeyStoreError {}

impl From<sqlx::Error> for ApiKeyStoreError {
    fn from(e: sqlx::Error) -> Self {
        match crate::db_error::classify(&e) {
            Some(crate::db_error::DbConstraint::Unique)
                if matches!(
                    e.as_database_error().and_then(|db| db.constraint()),
                    Some(
                        "rate_limits_global_identity_unique"
                            | "rate_limits_connection_identity_unique"
                    )
                ) =>
            {
                ApiKeyStoreError::DuplicateRateLimit
            }
            // 23503 = foreign_key_violation -> the usage_pool_id doesn't exist,
            // unless the FK that failed is the rate limit's connection_id.
            Some(crate::db_error::DbConstraint::ForeignKey) => {
                if e.as_database_error().and_then(|db| db.constraint())
                    == Some("rate_limits_connection_fk")
                {
                    ApiKeyStoreError::InvalidRateLimitConnection
                } else {
                    ApiKeyStoreError::InvalidUsagePool
                }
            }
            _ => ApiKeyStoreError::Internal(e.to_string()),
        }
    }
}

/// Only transport failures are safe reasons to retry the authentication read.
/// Pool exhaustion, query cancellation, schema errors and permission failures
/// should surface immediately rather than amplify load during an outage.
fn authentication_read_retryable(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Io(_) | sqlx::Error::Tls(_) | sqlx::Error::Protocol(_) => true,
        sqlx::Error::Database(error) => error.code().is_some_and(|code| {
            code.starts_with("08") || matches!(code.as_ref(), "57P01" | "57P02" | "57P03")
        }),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait ApiKeyStore: Send + Sync {
    /// Insert a fully-formed key (its `key_hash` already computed by the caller).
    async fn insert_raw(&self, key: ApiKey) -> Result<(), ApiKeyStoreError>;

    /// Mint a new key. Returns the raw secret once (see [`CreatedApiKey`]), and
    /// only after the key is durably persisted.
    async fn create(
        &self,
        description: String,
        permission: ApiKeyPermission,
        labels: HashMap<String, String>,
        usage_pool_id: Option<String>,
    ) -> Result<CreatedApiKey, ApiKeyStoreError>;

    /// Update mutable fields, identified by `key_hash`.
    async fn update(
        &self,
        key_hash: &str,
        description: Option<String>,
        usage_pool_id: Option<Option<String>>,
        permission: Option<ApiKeyPermission>,
    ) -> Result<Option<Arc<ApiKey>>, ApiKeyStoreError>;

    /// Resolve a *raw presented token* to a stored key (hashes then looks up).
    /// Used on the authentication hot path.
    async fn authenticate(&self, raw_token: &str) -> Option<Arc<ApiKey>>;

    /// Authenticate without conflating storage unavailability with an invalid
    /// key. Custom/in-memory stores retain their existing behavior by default;
    /// persistent stores override this to preserve database errors.
    async fn authenticate_checked(
        &self,
        raw_token: &str,
    ) -> Result<Option<Arc<ApiKey>>, ApiKeyStoreError> {
        Ok(self.authenticate(raw_token).await)
    }

    /// Look up a key by its `key_hash` (admin operations).
    async fn get(&self, key_hash: &str) -> Option<Arc<ApiKey>>;

    async fn list(&self) -> Vec<Arc<ApiKey>>;

    /// Fetch at most `fetch` keys strictly after `cursor` in the stable
    /// newest-first `(created_at, key_hash)` ordering. Callers request
    /// `limit + 1` rows to determine whether another page exists.
    async fn list_page(
        &self,
        cursor: Option<&StablePageCursor>,
        fetch: i64,
    ) -> Result<Vec<Arc<ApiKey>>, ApiKeyStoreError>;

    /// Select at most `fetch` keys whose immutable labels contain every
    /// requested key/value pair and, when supplied, the grouping label key and
    /// explicit API-key allowlist.
    /// PostgreSQL implements this with ordinary JSONB predicates; no GIN index
    /// or usage-ledger label copy is required.
    async fn select_by_labels(
        &self,
        labels: &HashMap<String, String>,
        required_label_key: Option<&str>,
        api_key_hashes: &[String],
        fetch: i64,
    ) -> Result<Vec<Arc<ApiKey>>, ApiKeyStoreError>;

    /// Atomically create a key and any co-located Postgres daily budget and
    /// rate-limit rows. `Ok(None)` means this store does not provide the
    /// co-located optimization, so callers must use the regular operations.
    #[allow(clippy::too_many_arguments)]
    async fn create_with_quota_definitions(
        &self,
        _description: String,
        _permission: ApiKeyPermission,
        _labels: HashMap<String, String>,
        _usage_pool_id: Option<String>,
        _daily_token_budget: Option<u64>,
        _rate_limits: Vec<ApiKeyRateLimitInput>,
        _usage_budgets: Option<Vec<ApiKeyBudgetInput>>,
    ) -> Result<Option<CreatedApiKey>, ApiKeyStoreError> {
        Ok(None)
    }

    /// Atomically update a key and its Postgres-backed quota definitions. The outer
    /// `Option` has the same unsupported meaning as
    /// [`ApiKeyStore::create_with_quota_definitions`]; the inner option is the
    /// usual "key found" result.
    #[allow(clippy::too_many_arguments)]
    async fn update_with_quota_definitions(
        &self,
        _key_hash: &str,
        _description: Option<String>,
        _usage_pool_id: Option<Option<String>>,
        _permission: Option<ApiKeyPermission>,
        _daily_token_budget: Option<Option<u64>>,
        _rate_limits: Option<Vec<ApiKeyRateLimitInput>>,
        _usage_budgets: Option<Vec<ApiKeyBudgetInput>>,
    ) -> Result<Option<Option<Arc<ApiKey>>>, ApiKeyStoreError> {
        Ok(None)
    }

    /// Backwards-compatible daily-budget-only form. New callers should use
    /// [`Self::update_with_quota_definitions`] so every direct quota mutation
    /// shares one transaction.
    async fn update_with_daily_budget(
        &self,
        key_hash: &str,
        description: Option<String>,
        usage_pool_id: Option<Option<String>>,
        permission: Option<ApiKeyPermission>,
        daily_token_budget: Option<Option<u64>>,
    ) -> Result<Option<Option<Arc<ApiKey>>>, ApiKeyStoreError> {
        self.update_with_quota_definitions(
            key_hash,
            description,
            usage_pool_id,
            permission,
            daily_token_budget,
            None,
            None,
        )
        .await
    }

    /// Revoke a key by its `key_hash`.
    async fn revoke(&self, key_hash: &str) -> Result<bool, ApiKeyStoreError>;

    /// Atomically delete a key and its co-located quota definitions. See
    /// [`ApiKeyStore::create_with_quota_definitions`] for the outer `Option`.
    async fn revoke_with_quota_definitions(
        &self,
        _key_hash: &str,
    ) -> Result<Option<bool>, ApiKeyStoreError> {
        Ok(None)
    }

    /// Record token usage. No-op in phase 1 (usage backend disabled); retained
    /// so request handlers need not branch on backend availability.
    async fn record_usage(&self, _key_hash: &str, _prompt: u64, _completion: u64, _total: u64) {}
}

// ---------------------------------------------------------------------------
// In-memory implementation
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct InMemoryApiKeyStore {
    keys: Arc<DashMap<String, Arc<ApiKey>>>, // keyed by key_hash
    hasher: KeyHasher,
}

impl InMemoryApiKeyStore {
    // Dev/tests are single-environment, so the in-memory store is not
    // environment-scoped; the tenancy boundary lives in the Postgres stores.
    pub fn new(hasher: KeyHasher) -> Self {
        Self {
            keys: Arc::new(DashMap::new()),
            hasher,
        }
    }
}

#[async_trait]
impl ApiKeyStore for InMemoryApiKeyStore {
    async fn insert_raw(&self, key: ApiKey) -> Result<(), ApiKeyStoreError> {
        self.keys.insert(key.key_hash.clone(), Arc::new(key));
        Ok(())
    }

    async fn create(
        &self,
        description: String,
        permission: ApiKeyPermission,
        labels: HashMap<String, String>,
        usage_pool_id: Option<String>,
    ) -> Result<CreatedApiKey, ApiKeyStoreError> {
        let raw_key = format!("ck-{}", Uuid::new_v4().simple());
        let api_key = Arc::new(ApiKey {
            key_hash: self.hasher.hash(&raw_key),
            key_prefix: key_prefix(&raw_key),
            description,
            created_at: chrono::Utc::now(),
            permission,
            labels,
            usage_pool_id,
            daily_token_budget: None,
        });
        self.keys.insert(api_key.key_hash.clone(), api_key.clone());
        Ok(CreatedApiKey { raw_key, api_key })
    }

    async fn update(
        &self,
        key_hash: &str,
        description: Option<String>,
        usage_pool_id: Option<Option<String>>,
        permission: Option<ApiKeyPermission>,
    ) -> Result<Option<Arc<ApiKey>>, ApiKeyStoreError> {
        let old = match self.keys.get(key_hash) {
            Some(e) => e.value().clone(),
            None => return Ok(None),
        };
        let updated = Arc::new(ApiKey {
            key_hash: old.key_hash.clone(),
            key_prefix: old.key_prefix.clone(),
            description: description.unwrap_or_else(|| old.description.clone()),
            created_at: old.created_at,
            permission: permission.unwrap_or_else(|| old.permission.clone()),
            labels: old.labels.clone(),
            usage_pool_id: usage_pool_id.unwrap_or_else(|| old.usage_pool_id.clone()),
            daily_token_budget: None,
        });
        self.keys.insert(key_hash.to_string(), updated.clone());
        Ok(Some(updated))
    }

    async fn authenticate(&self, raw_token: &str) -> Option<Arc<ApiKey>> {
        let hash = self.hasher.hash(raw_token);
        self.keys.get(&hash).map(|e| e.value().clone())
    }

    async fn authenticate_checked(
        &self,
        raw_token: &str,
    ) -> Result<Option<Arc<ApiKey>>, ApiKeyStoreError> {
        // Keep the memory hot path to one boxed trait future as well.
        let hash = self.hasher.hash(raw_token);
        Ok(self.keys.get(&hash).map(|e| e.value().clone()))
    }

    async fn get(&self, key_hash: &str) -> Option<Arc<ApiKey>> {
        self.keys.get(key_hash).map(|e| e.value().clone())
    }

    async fn list(&self) -> Vec<Arc<ApiKey>> {
        self.keys.iter().map(|e| e.value().clone()).collect()
    }

    async fn list_page(
        &self,
        cursor: Option<&StablePageCursor>,
        fetch: i64,
    ) -> Result<Vec<Arc<ApiKey>>, ApiKeyStoreError> {
        let mut keys: Vec<_> = self
            .keys
            .iter()
            .map(|entry| entry.value().clone())
            .filter(|key| match cursor {
                None => true,
                Some(cursor) => {
                    (key.created_at, key.key_hash.as_str())
                        < (cursor.created_at, cursor.id.as_str())
                }
            })
            .collect();
        keys.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.key_hash.cmp(&a.key_hash))
        });
        keys.truncate(fetch.max(0) as usize);
        Ok(keys)
    }

    async fn select_by_labels(
        &self,
        labels: &HashMap<String, String>,
        required_label_key: Option<&str>,
        api_key_hashes: &[String],
        fetch: i64,
    ) -> Result<Vec<Arc<ApiKey>>, ApiKeyStoreError> {
        let mut keys: Vec<_> = self
            .keys
            .iter()
            .map(|entry| entry.value().clone())
            .filter(|key| {
                labels
                    .iter()
                    .all(|(name, value)| key.labels.get(name) == Some(value))
                    && required_label_key.is_none_or(|name| key.labels.contains_key(name))
                    && (api_key_hashes.is_empty()
                        || api_key_hashes.iter().any(|hash| hash == &key.key_hash))
            })
            .collect();
        keys.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.key_hash.cmp(&a.key_hash))
        });
        keys.truncate(fetch.max(0) as usize);
        Ok(keys)
    }

    async fn revoke(&self, key_hash: &str) -> Result<bool, ApiKeyStoreError> {
        Ok(self.keys.remove(key_hash).is_some())
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL implementation
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct PostgresApiKeyStore {
    pool: sqlx::PgPool,
    hasher: KeyHasher,
    /// Tenancy scope: every query is filtered by this environment (the DB may be
    /// shared across environments). Set once from `CHALK_ENVIRONMENT_ID`.
    environment_id: String,
}

/// Serialize a permission for storage. An unreadable permission has no stored
/// form: it would serialize as `{}`, which grants everything.
///
/// A key restricted only by `route_ids` is stored with an explicit empty
/// `connection_scopes`. Routers that predate route grants ignore `route_ids`
/// and would otherwise read `{}`; an empty scope list denies them everything.
fn stored_permission(permission: &ApiKeyPermission) -> Result<serde_json::Value, ApiKeyStoreError> {
    if permission.unreadable {
        return Err(ApiKeyStoreError::Internal(
            "refusing to store an unreadable API key permission".into(),
        ));
    }
    if permission.route_ids.is_some()
        && permission.connection_scopes.is_none()
        && !permission.has_legacy_fields()
    {
        let mut permission = permission.clone();
        permission.connection_scopes = Some(Vec::new());
        return Ok(serde_json::to_value(permission).unwrap_or_default());
    }
    Ok(serde_json::to_value(permission).unwrap_or_default())
}

impl PostgresApiKeyStore {
    /// Wrap an existing pool. Tables are provisioned by migrations before boot.
    /// Resolve the owning environment from a credential, failing closed on duplicates.
    pub async fn authenticate_cluster(
        pool: &sqlx::PgPool,
        hasher: &KeyHasher,
        token: &str,
    ) -> Result<Option<(String, Arc<ApiKey>)>, ApiKeyStoreError> {
        use sqlx::Row;
        let rows = sqlx::query("SELECT * FROM chalk_router.api_keys WHERE key_hash=$1 LIMIT 2")
            .bind(hasher.hash(token))
            .fetch_all(pool)
            .await?;
        if rows.len() > 1 {
            return Err(ApiKeyStoreError::Internal(
                "ambiguous credential identity".into(),
            ));
        }
        Ok(rows
            .first()
            .map(|row| (row.get("environment_id"), Self::row_to_api_key(row))))
    }

    pub fn new(pool: sqlx::PgPool, hasher: KeyHasher, environment_id: String) -> Self {
        Self {
            pool,
            hasher,
            environment_id,
        }
    }

    pub fn pool(&self) -> &sqlx::PgPool {
        &self.pool
    }

    async fn fetch_by_hash(&self, hash: &str) -> Result<Option<Arc<ApiKey>>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT * FROM chalk_router.api_keys WHERE key_hash = $1 AND environment_id = $2",
        )
        .bind(hash)
        .bind(&self.environment_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(Self::row_to_api_key))
    }

    fn row_to_api_key(row: &sqlx::postgres::PgRow) -> Arc<ApiKey> {
        use sqlx::Row;
        let key_prefix: String = row.get("key_prefix");
        let permission = ApiKeyPermission::from_stored(&key_prefix, row.get("permission"));
        let labels_json: serde_json::Value = row.get("labels");
        let labels: HashMap<String, String> =
            serde_json::from_value(labels_json).unwrap_or_default();

        Arc::new(ApiKey {
            key_hash: row.get("key_hash"),
            key_prefix,
            description: row.get("description"),
            created_at: row.get("created_at"),
            permission,
            labels,
            usage_pool_id: row
                .try_get::<Option<String>, _>("usage_pool_id")
                .ok()
                .flatten(),
            daily_token_budget: None,
        })
    }
}

#[async_trait]
impl ApiKeyStore for PostgresApiKeyStore {
    async fn insert_raw(&self, key: ApiKey) -> Result<(), ApiKeyStoreError> {
        let permission = stored_permission(&key.permission)?;
        let labels = serde_json::to_value(&key.labels).unwrap_or_default();
        sqlx::query(
            "INSERT INTO chalk_router.api_keys (environment_id, key_hash, key_prefix, description, created_at, permission, labels, usage_pool_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (environment_id, key_hash) DO NOTHING",
        )
        .bind(&self.environment_id)
        .bind(&key.key_hash)
        .bind(&key.key_prefix)
        .bind(&key.description)
        .bind(key.created_at)
        .bind(permission)
        .bind(labels)
        .bind(key.usage_pool_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn create(
        &self,
        description: String,
        permission: ApiKeyPermission,
        labels: HashMap<String, String>,
        usage_pool_id: Option<String>,
    ) -> Result<CreatedApiKey, ApiKeyStoreError> {
        let raw_key = format!("ck-{}", Uuid::new_v4().simple());
        let key_hash = self.hasher.hash(&raw_key);
        let prefix = key_prefix(&raw_key);
        let now = chrono::Utc::now();
        let permission_json = stored_permission(&permission)?;
        let labels_json = serde_json::to_value(&labels).unwrap_or_default();

        // Persist first; the raw secret is only surfaced if the insert succeeds.
        sqlx::query(
            "INSERT INTO chalk_router.api_keys (environment_id, key_hash, key_prefix, description, created_at, permission, labels, usage_pool_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&self.environment_id)
        .bind(&key_hash)
        .bind(&prefix)
        .bind(&description)
        .bind(now)
        .bind(&permission_json)
        .bind(&labels_json)
        .bind(usage_pool_id.as_deref())
        .execute(&self.pool)
        .await?;

        let api_key = Arc::new(ApiKey {
            key_hash,
            key_prefix: prefix,
            description,
            created_at: now,
            permission,
            labels,
            usage_pool_id,
            daily_token_budget: None,
        });
        Ok(CreatedApiKey { raw_key, api_key })
    }

    async fn update(
        &self,
        key_hash: &str,
        description: Option<String>,
        usage_pool_id: Option<Option<String>>,
        permission: Option<ApiKeyPermission>,
    ) -> Result<Option<Arc<ApiKey>>, ApiKeyStoreError> {
        let mut sets: Vec<String> = Vec::new();
        let mut param_idx = 3u32; // $1 = key_hash, $2 = environment_id

        if description.is_some() {
            sets.push(format!("description = ${param_idx}"));
            param_idx += 1;
        }
        if usage_pool_id.is_some() {
            sets.push(format!("usage_pool_id = ${param_idx}"));
            param_idx += 1;
        }
        if permission.is_some() {
            sets.push(format!("permission = ${param_idx}"));
        }
        if sets.is_empty() {
            return Ok(self.get(key_hash).await);
        }

        let sql = format!(
            "UPDATE chalk_router.api_keys SET {} WHERE key_hash = $1 AND environment_id = $2 RETURNING *",
            sets.join(", ")
        );
        // `sets` contains only the fixed column assignments selected above.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(key_hash)
            .bind(&self.environment_id);

        if let Some(ref desc) = description {
            query = query.bind(desc);
        }
        if let Some(pool) = usage_pool_id {
            query = query.bind(pool);
        }
        if let Some(permission) = permission {
            query = query.bind(stored_permission(&permission)?);
        }

        let row = query.fetch_optional(&self.pool).await?;
        Ok(row.map(|row| Self::row_to_api_key(&row)))
    }

    async fn authenticate(&self, raw_token: &str) -> Option<Arc<ApiKey>> {
        self.authenticate_checked(raw_token).await.ok().flatten()
    }

    async fn authenticate_checked(
        &self,
        raw_token: &str,
    ) -> Result<Option<Arc<ApiKey>>, ApiKeyStoreError> {
        let hash = self.hasher.hash(raw_token);
        for attempt in 0..2 {
            let result = self.fetch_by_hash(&hash).await;
            match result {
                Ok(key) => return Ok(key),
                // A connection may die immediately after its release check.
                // This lookup has no writes; retry it once on a fresh checkout.
                // Never apply this retry to quota admission or settlement.
                Err(error) if attempt == 0 && authentication_read_retryable(&error) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        unreachable!("each final authentication attempt returns")
    }

    async fn get(&self, key_hash: &str) -> Option<Arc<ApiKey>> {
        self.fetch_by_hash(key_hash).await.ok().flatten()
    }

    async fn list(&self) -> Vec<Arc<ApiKey>> {
        sqlx::query("SELECT * FROM chalk_router.api_keys WHERE environment_id = $1")
            .bind(&self.environment_id)
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default()
            .iter()
            .map(Self::row_to_api_key)
            .collect()
    }

    async fn list_page(
        &self,
        cursor: Option<&StablePageCursor>,
        fetch: i64,
    ) -> Result<Vec<Arc<ApiKey>>, ApiKeyStoreError> {
        let rows = sqlx::query(
            "SELECT * FROM chalk_router.api_keys \
             WHERE environment_id = $1 \
               AND ($2::timestamptz IS NULL OR (created_at, key_hash) < ($2, $3)) \
             ORDER BY created_at DESC, key_hash DESC LIMIT $4",
        )
        .bind(&self.environment_id)
        .bind(cursor.map(|c| c.created_at))
        .bind(cursor.map(|c| c.id.as_str()))
        .bind(fetch.max(0))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(Self::row_to_api_key).collect())
    }

    async fn select_by_labels(
        &self,
        labels: &HashMap<String, String>,
        required_label_key: Option<&str>,
        api_key_hashes: &[String],
        fetch: i64,
    ) -> Result<Vec<Arc<ApiKey>>, ApiKeyStoreError> {
        let labels_json = serde_json::to_value(labels).unwrap_or_default();
        let rows = sqlx::query(
            "SELECT * FROM chalk_router.api_keys \
             WHERE environment_id = $1 \
               AND labels @> $2::jsonb \
               AND ($3::text IS NULL OR jsonb_exists(labels, $3)) \
               AND (cardinality($4::text[]) = 0 OR key_hash = ANY($4)) \
             ORDER BY created_at DESC, key_hash DESC LIMIT $5",
        )
        .bind(&self.environment_id)
        .bind(labels_json)
        .bind(required_label_key)
        .bind(api_key_hashes)
        .bind(fetch.max(0))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(Self::row_to_api_key).collect())
    }

    async fn create_with_quota_definitions(
        &self,
        description: String,
        permission: ApiKeyPermission,
        labels: HashMap<String, String>,
        usage_pool_id: Option<String>,
        daily_token_budget: Option<u64>,
        rate_limits: Vec<ApiKeyRateLimitInput>,
        usage_budgets: Option<Vec<ApiKeyBudgetInput>>,
    ) -> Result<Option<CreatedApiKey>, ApiKeyStoreError> {
        let raw_key = format!("ck-{}", Uuid::new_v4().simple());
        let key_hash = self.hasher.hash(&raw_key);
        let prefix = key_prefix(&raw_key);
        let now = chrono::Utc::now();
        let permission_json = stored_permission(&permission)?;
        let labels_json = serde_json::to_value(&labels).unwrap_or_default();
        let policy_json =
            serde_json::to_value(api_key_rate_limit_policies(&key_hash, &rate_limits))
                .unwrap_or_default();
        let budget_rows = usage_budgets.unwrap_or_else(|| {
            daily_token_budget
                .map(|token_limit| {
                    vec![ApiKeyBudgetInput {
                        token_limit,
                        period: "daily".to_string(),
                        enabled: true,
                    }]
                })
                .unwrap_or_default()
        });
        let mut budget_json_rows = Vec::with_capacity(budget_rows.len());
        for budget in budget_rows {
            let limit_amount = i64::try_from(budget.token_limit).map_err(|_| {
                ApiKeyStoreError::Internal("usage budget token_limit must fit BIGINT".to_string())
            })?;
            budget_json_rows.push(serde_json::json!({
                "id": Uuid::new_v4().to_string(),
                "quota_policy_id": Uuid::new_v4().to_string(),
                "period": budget.period,
                "limit_amount": limit_amount,
                "enabled": budget.enabled,
            }));
        }
        let budget_json = serde_json::Value::Array(budget_json_rows);

        // One statement means the key, source definitions, and checked quota
        // bridges commit or roll back together.
        sqlx::query(
            "WITH policy_rows AS MATERIALIZED ( \
                SELECT * FROM jsonb_to_recordset($10::jsonb) AS policy( \
                    quota_policy_id text, id text, limit_type text, \
                    limit_value bigint, burst_value bigint, connection_id text, models jsonb, enabled boolean \
                ) \
             ), budget_rows AS MATERIALIZED ( \
                SELECT * FROM jsonb_to_recordset($9::jsonb) AS budget( \
                    id text, quota_policy_id text, period text, limit_amount bigint, enabled boolean \
                ) \
             ), inserted AS ( \
                INSERT INTO chalk_router.api_keys \
                    (environment_id, key_hash, key_prefix, description, created_at, permission, labels, usage_pool_id) \
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                RETURNING key_hash \
             ), budget AS ( \
                INSERT INTO chalk_router.budget_limits \
                    (id, environment_id, scope, scope_id, period, window_type, window_size, limit_kind, limit_amount, enabled) \
                SELECT budget.id, $1, 'api_key', key_hash, budget.period, 'calendar', \
                    CASE budget.period WHEN 'weekly' THEN INTERVAL '7 days' WHEN 'monthly' THEN INTERVAL '1 month' ELSE INTERVAL '1 day' END, \
                    'tokens', budget.limit_amount, budget.enabled \
                FROM inserted CROSS JOIN budget_rows AS budget \
                RETURNING id, created_at \
             ), budget_policy AS ( \
                INSERT INTO chalk_router.quota_policies \
                    (id, environment_id, budget_limit_id, created_at) \
                SELECT rows.quota_policy_id, $1, budget.id, budget.created_at \
                FROM budget_rows AS rows JOIN budget ON budget.id = rows.id \
             ), rate_limits AS ( \
                INSERT INTO chalk_router.rate_limits \
                    (environment_id, id, limit_type, limit_value, connection_id, \
                     models, burst_value, scope, scope_id, enabled, created_at) \
                SELECT $1, policy.id, policy.limit_type, policy.limit_value, \
                       policy.connection_id, \
                       ARRAY(SELECT jsonb_array_elements_text(policy.models)), \
                       policy.burst_value, 'api_key', inserted.key_hash, policy.enabled, $5 \
                FROM inserted \
                CROSS JOIN policy_rows AS policy \
                RETURNING id \
             ), rate_policies AS ( \
                INSERT INTO chalk_router.quota_policies \
                    (id, environment_id, rate_limit_id, created_at) \
                SELECT policy.quota_policy_id, $1, policy.id, $5 \
                FROM policy_rows AS policy \
                JOIN rate_limits AS limits ON limits.id = policy.id \
             ) \
             SELECT key_hash FROM inserted",
        )
        .bind(&self.environment_id)
        .bind(&key_hash)
        .bind(&prefix)
        .bind(&description)
        .bind(now)
        .bind(permission_json)
        .bind(labels_json)
        .bind(usage_pool_id.as_deref())
        .bind(budget_json)
        .bind(policy_json)
        .fetch_one(&self.pool)
        .await?;

        let api_key = Arc::new(ApiKey {
            key_hash,
            key_prefix: prefix,
            description,
            created_at: now,
            permission,
            labels,
            usage_pool_id,
            daily_token_budget: None,
        });
        Ok(Some(CreatedApiKey { raw_key, api_key }))
    }

    async fn update_with_quota_definitions(
        &self,
        key_hash: &str,
        description: Option<String>,
        usage_pool_id: Option<Option<String>>,
        permission: Option<ApiKeyPermission>,
        daily_token_budget: Option<Option<u64>>,
        rate_limits: Option<Vec<ApiKeyRateLimitInput>>,
        usage_budgets: Option<Vec<ApiKeyBudgetInput>>,
    ) -> Result<Option<Option<Arc<ApiKey>>>, ApiKeyStoreError> {
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query(
            "UPDATE chalk_router.api_keys SET \
                description = CASE WHEN $3::boolean THEN $4::text ELSE description END, \
                usage_pool_id = CASE WHEN $5::boolean THEN $6::text ELSE usage_pool_id END, \
                permission = CASE WHEN $7::boolean THEN $8::jsonb ELSE permission END \
             WHERE key_hash = $1 AND environment_id = $2 RETURNING *",
        )
        .bind(key_hash)
        .bind(&self.environment_id)
        .bind(description.is_some())
        .bind(description.as_deref())
        .bind(usage_pool_id.is_some())
        .bind(usage_pool_id.clone().flatten())
        .bind(permission.is_some())
        .bind(permission.as_ref().map(stored_permission).transpose()?)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(row) = row else {
            transaction.commit().await?;
            return Ok(Some(None));
        };
        let updated = Self::row_to_api_key(&row);

        if let Some(budgets) = usage_budgets {
            sqlx::query(
                "DELETE FROM chalk_router.quota_policies q USING chalk_router.budget_limits b \
                 WHERE q.environment_id = $1 AND q.budget_limit_id = b.id \
                   AND b.environment_id = $1 AND b.scope = 'api_key' AND b.scope_id = $2 \
                   AND b.limit_kind = 'tokens' AND b.window_type = 'calendar' \
                   AND b.timezone = 'UTC' AND b.period IN ('daily', 'weekly', 'monthly')",
            )
            .bind(&self.environment_id)
            .bind(key_hash)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "DELETE FROM chalk_router.budget_limits \
                 WHERE environment_id = $1 AND scope = 'api_key' AND scope_id = $2 \
                   AND limit_kind = 'tokens' AND window_type = 'calendar' \
                   AND timezone = 'UTC' AND period IN ('daily', 'weekly', 'monthly')",
            )
            .bind(&self.environment_id)
            .bind(key_hash)
            .execute(&mut *transaction)
            .await?;
            for budget in budgets {
                let budget_id = Uuid::new_v4().to_string();
                sqlx::query(
                    "INSERT INTO chalk_router.budget_limits \
                     (id, environment_id, scope, scope_id, period, window_type, window_size, limit_kind, limit_amount, enabled) \
                     VALUES ($1, $2, 'api_key', $3, $4, 'calendar', \
                         CASE $4 WHEN 'weekly' THEN INTERVAL '7 days' WHEN 'monthly' THEN INTERVAL '1 month' ELSE INTERVAL '1 day' END, \
                         'tokens', $5, $6)",
                )
                .bind(&budget_id)
                .bind(&self.environment_id)
                .bind(key_hash)
                .bind(&budget.period)
                .bind(i64::try_from(budget.token_limit).map_err(|_| sqlx::Error::Protocol("token_limit must fit BIGINT".into()))?)
                .bind(budget.enabled)
                .execute(&mut *transaction)
                .await?;
                sqlx::query(
                    "INSERT INTO chalk_router.quota_policies (id, environment_id, budget_limit_id) \
                     VALUES ($1, $2, $3)",
                )
                .bind(Uuid::new_v4().to_string())
                .bind(&self.environment_id)
                .bind(budget_id)
                .execute(&mut *transaction)
                .await?;
            }
        } else if let Some(amount) = daily_token_budget {
            sqlx::query(
                "DELETE FROM chalk_router.budget_limits \
                 WHERE environment_id = $1 AND scope = 'api_key' AND scope_id = $2 \
                   AND limit_kind = 'tokens' AND window_type = 'calendar' \
                   AND window_size = INTERVAL '1 day'",
            )
            .bind(&self.environment_id)
            .bind(key_hash)
            .execute(&mut *transaction)
            .await?;
            if let Some(amount) = amount {
                let budget_id = Uuid::new_v4().to_string();
                sqlx::query(
                    "INSERT INTO chalk_router.budget_limits \
                     (id, environment_id, scope, scope_id, window_type, window_size, limit_kind, limit_amount, enabled) \
                     VALUES ($1, $2, 'api_key', $3, 'calendar', INTERVAL '1 day', 'tokens', $4, true)",
                )
                .bind(&budget_id)
                .bind(&self.environment_id)
                .bind(key_hash)
                .bind(amount as i64)
                .execute(&mut *transaction)
                .await?;
                sqlx::query(
                    "INSERT INTO chalk_router.quota_policies (id, environment_id, budget_limit_id) \
                     VALUES ($1, $2, $3)",
                )
                .bind(Uuid::new_v4().to_string())
                .bind(&self.environment_id)
                .bind(budget_id)
                .execute(&mut *transaction)
                .await?;
            }
        }

        if let Some(rate_limits) = rate_limits {
            sqlx::query(
                "DELETE FROM chalk_router.rate_limits \
                 WHERE environment_id = $1 AND scope = 'api_key' AND scope_id = $2",
            )
            .bind(&self.environment_id)
            .bind(key_hash)
            .execute(&mut *transaction)
            .await?;
            for policy in api_key_rate_limit_policies(key_hash, &rate_limits) {
                sqlx::query(
                    "INSERT INTO chalk_router.rate_limits \
                     (environment_id, id, limit_type, limit_value, connection_id, models, burst_value, scope, scope_id, enabled, created_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, 'api_key', $8, $9, $10)",
                )
                .bind(&self.environment_id)
                .bind(&policy.id)
                .bind(&policy.limit_type)
                .bind(policy.limit_value as i64)
                .bind(&policy.connection_id)
                .bind(&policy.models)
                .bind(policy.burst_value.map(|value| value as i64))
                .bind(key_hash)
                .bind(policy.enabled)
                .bind(policy.created_at)
                .execute(&mut *transaction)
                .await?;
                sqlx::query(
                    "INSERT INTO chalk_router.quota_policies (id, environment_id, rate_limit_id, created_at) \
                     VALUES ($1, $2, $3, $4)",
                )
                .bind(&policy.quota_policy_id)
                .bind(&self.environment_id)
                .bind(&policy.id)
                .bind(policy.created_at)
                .execute(&mut *transaction)
                .await?;
            }
        }
        transaction.commit().await?;
        Ok(Some(Some(updated)))
    }

    async fn revoke(&self, key_hash: &str) -> Result<bool, ApiKeyStoreError> {
        Ok(sqlx::query(
            "DELETE FROM chalk_router.api_keys WHERE key_hash = $1 AND environment_id = $2",
        )
        .bind(key_hash)
        .bind(&self.environment_id)
        .execute(&self.pool)
        .await
        .map(|r| r.rows_affected() > 0)?)
    }

    async fn revoke_with_quota_definitions(
        &self,
        key_hash: &str,
    ) -> Result<Option<bool>, ApiKeyStoreError> {
        let deleted: Option<bool> = sqlx::query_scalar(
            "WITH removed_key AS ( \
                DELETE FROM chalk_router.api_keys \
                WHERE key_hash = $1 AND environment_id = $2 RETURNING key_hash \
             ), removed_budget AS ( \
                DELETE FROM chalk_router.budget_limits b USING removed_key k \
                WHERE b.environment_id = $2 AND b.scope = 'api_key' AND b.scope_id = k.key_hash \
                  AND b.limit_kind = 'tokens' AND b.window_type = 'calendar' \
             ), removed_rate_limits AS ( \
                DELETE FROM chalk_router.rate_limits r USING removed_key k \
                WHERE r.environment_id = $2 AND r.scope = 'api_key' AND r.scope_id = k.key_hash \
             ) SELECT true FROM removed_key",
        )
        .bind(key_hash)
        .bind(&self.environment_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(Some(deleted.unwrap_or(false)))
    }
}

#[cfg(test)]
mod permission_tests {
    use super::*;

    fn perm(provider: Option<&str>, models: Option<&[&str]>) -> ApiKeyPermission {
        ApiKeyPermission {
            provider: provider.map(str::to_string),
            models: models.map(|m| m.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }
    }

    fn perm_conn(connection_id: &str) -> ApiKeyPermission {
        ApiKeyPermission {
            connection_id: Some(connection_id.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn authentication_does_not_retry_pool_timeouts_or_schema_errors() {
        assert!(!authentication_read_retryable(&sqlx::Error::PoolTimedOut));
        assert!(!authentication_read_retryable(&sqlx::Error::PoolClosed));
        assert!(!authentication_read_retryable(
            &sqlx::Error::ColumnNotFound("missing".into())
        ));
        assert!(authentication_read_retryable(&sqlx::Error::Io(
            std::io::Error::from(std::io::ErrorKind::ConnectionReset)
        )));
        assert!(authentication_read_retryable(&sqlx::Error::Protocol(
            "connection lost".into()
        )));
    }

    #[test]
    fn stored_permission_that_does_not_parse_denies_the_key() {
        for stored in [
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!({ "models": "openai/gpt-4o" }),
            serde_json::json!({ "connection_scopes": [{ "models": [] }] }),
            serde_json::json!({ "future": 1 }),
            serde_json::json!({ "routes": { "refs": [{ "public_model_id": "chalk/a" }] } }),
            serde_json::json!({
                "route_ids": ["route-a"],
                "connection_id": "id-openai",
                "future": 1
            }),
            serde_json::json!({
                "connection_scopes": [{ "connection_id": "id-openai", "modelz": ["gpt-4o"] }]
            }),
            serde_json::json!({
                "route_ids": ["route-a"],
                "connection_scopes": [
                    { "connection_id": "id-openai", "models": ["gpt-4o"] },
                    { "connection_id": "other", "models": [], "future": 1 }
                ]
            }),
            serde_json::json!({ "unreadable": false }),
            serde_json::json!({ "connection_id": "id-openai", "unreadable": true }),
        ] {
            let p = ApiKeyPermission::from_stored("ck-test", stored.clone());
            assert!(p.unreadable, "{stored}");
            assert!(p.is_restricted(), "{stored}");
            assert!(matches!(
                p.allows("openai", Some("id-openai"), "openai/gpt-4o"),
                Err(ApiKeyError::PermissionUnreadable)
            ));
            assert!(matches!(
                p.allows_route(
                    &resolution("openai", "id-openai", Some("openai/")),
                    "gpt-4o"
                ),
                Err(ApiKeyError::PermissionUnreadable)
            ));
            assert!(matches!(
                p.allows_egress(None, "openai", Some("id-openai")),
                Err(ApiKeyError::PermissionUnreadable)
            ));
            assert!(!p.grants_model_route(Some("route-a")), "{stored}");
            assert!(matches!(
                p.allows_route(&routed("route-a"), "chalk/a"),
                Err(ApiKeyError::PermissionUnreadable)
            ));
        }
    }

    #[test]
    fn stored_permission_that_parses_is_read_as_written() {
        let empty = ApiKeyPermission::from_stored("ck-test", serde_json::json!({}));
        assert!(!empty.unreadable);
        assert!(!empty.is_restricted());

        let legacy = ApiKeyPermission::from_stored(
            "ck-test",
            serde_json::json!({ "connection_id": "id-openai" }),
        );
        assert!(!legacy.unreadable);
        assert_eq!(legacy.connection_id.as_deref(), Some("id-openai"));

        let route_id = "6b054d3d-ffde-4ef9-a43a-0e3842ab6702";
        let scoped = ApiKeyPermission::from_stored(
            "ck-test",
            serde_json::json!({
                "route_ids": [route_id],
                "connection_scopes": [{ "connection_id": "id-openai" }]
            }),
        );
        assert!(!scoped.unreadable);
        assert!(scoped.grants_model_route(Some(route_id)));
        assert!(scoped.allows("openai", Some("id-openai"), "gpt-4o").is_ok());
    }

    #[test]
    fn unreadable_permission_is_never_stored() {
        // Serialized, the marker vanishes and leaves `{}`: unrestricted.
        assert!(stored_permission(&ApiKeyPermission::unreadable()).is_err());
        assert_eq!(
            stored_permission(&perm_conn("id-openai")).unwrap(),
            serde_json::json!({ "connection_id": "id-openai" })
        );
    }

    fn scope(connection_id: &str, models: &[&str]) -> ApiKeyConnectionScope {
        ApiKeyConnectionScope {
            connection_id: connection_id.to_string(),
            models: models.iter().map(|model| model.to_string()).collect(),
        }
    }

    fn routed(route_id: &str) -> crate::backend::RouteResolution {
        crate::backend::RouteResolution {
            model_route_id: Some(route_id.to_string()),
            ..resolution("openai", "conn-a", None)
        }
    }

    #[test]
    fn route_only_key_is_stored_with_empty_connection_scopes() {
        // Routers that predate route grants ignore `route_ids`; an empty scope
        // list denies them everything instead of reading `{}`.
        let p = ApiKeyPermission {
            route_ids: Some(vec!["route-a".into()]),
            ..Default::default()
        };
        assert_eq!(
            stored_permission(&p).unwrap(),
            serde_json::json!({ "route_ids": ["route-a"], "connection_scopes": [] })
        );
        let scoped = ApiKeyPermission {
            connection_scopes: Some(vec![scope("conn-a", &[])]),
            ..p.clone()
        };
        assert_eq!(
            stored_permission(&scoped).unwrap(),
            serde_json::json!({
                "route_ids": ["route-a"],
                "connection_scopes": [{ "connection_id": "conn-a", "models": [] }]
            })
        );
        let read = ApiKeyPermission::from_stored("ck-test", stored_permission(&p).unwrap());
        assert!(read.grants_model_route(Some("route-a")));
        assert!(read
            .allows("openai", Some("conn-a"), "openai/gpt-4o")
            .is_err());
    }

    #[test]
    fn access_is_the_union_of_route_grants_and_connection_scopes() {
        let p = ApiKeyPermission {
            route_ids: Some(vec!["route-a".into()]),
            connection_scopes: Some(vec![scope("conn-a", &["openai/gpt-4o"])]),
            ..Default::default()
        };
        assert!(p.is_restricted());
        // The granted route, through any connection.
        assert!(p.allows_route(&routed("route-a"), "chalk/a").is_ok());
        assert!(p
            .allows_route(
                &crate::backend::RouteResolution {
                    model_route_id: Some("route-a".into()),
                    ..resolution("anthropic", "conn-z", None)
                },
                "chalk/a"
            )
            .is_ok());
        // Another route, even one served by a scoped connection.
        assert!(p.allows_route(&routed("route-b"), "chalk/b").is_err());
        // A route id with no route behind it (a concrete target) is denied.
        assert!(p
            .allows_route(&resolution("openai", "conn-a", None), "chalk/a")
            .is_err());
        // Direct models come from the scopes alone.
        assert!(p
            .allows_route(&resolution("openai", "conn-a", None), "openai/gpt-4o")
            .is_ok());
        assert!(p
            .allows_route(&resolution("openai", "conn-a", None), "openai/gpt-5")
            .is_err());
        // An explicit route id serves judge picks and route fallbacks.
        assert!(p
            .allows_model(
                &resolution("anthropic", "conn-z", None),
                "chalk/a",
                Some("route-a")
            )
            .is_ok());
    }

    #[test]
    fn connection_access_never_grants_a_route() {
        for p in [
            ApiKeyPermission {
                connection_scopes: Some(vec![scope("conn-a", &[])]),
                ..Default::default()
            },
            ApiKeyPermission {
                connection_scopes: Some(vec![scope("conn-a", &["chalk/a"])]),
                ..Default::default()
            },
            perm(Some("openai"), None),
            perm(None, Some(&["chalk/a"])),
            perm_conn("conn-a"),
        ] {
            assert!(
                p.allows_route(&routed("route-a"), "chalk/a").is_err(),
                "{p:?}"
            );
            assert!(!p.grants_model_route(Some("route-a")), "{p:?}");
        }
    }

    #[test]
    fn route_only_key_calls_no_direct_model() {
        for p in [
            ApiKeyPermission {
                route_ids: Some(vec!["route-a".into()]),
                ..Default::default()
            },
            ApiKeyPermission {
                route_ids: Some(Vec::new()),
                ..Default::default()
            },
        ] {
            assert!(p.is_restricted());
            assert!(p.allows("openai", Some("conn-a"), "openai/gpt-4o").is_err());
            assert!(p.allows_egress(None, "openai", Some("conn-a")).is_err());
        }
    }

    #[test]
    fn egress_for_a_granted_route_is_allowed_on_any_connection() {
        let p = ApiKeyPermission {
            route_ids: Some(vec!["route-a".into()]),
            connection_scopes: Some(vec![scope("conn-a", &[])]),
            ..Default::default()
        };
        assert!(p
            .allows_egress(Some("route-a"), "anthropic", Some("conn-z"))
            .is_ok());
        assert!(p
            .allows_egress(Some("route-b"), "anthropic", Some("conn-z"))
            .is_err());
        assert!(p.allows_egress(None, "openai", Some("conn-a")).is_ok());
        assert!(p.allows_egress(None, "anthropic", Some("conn-z")).is_err());
        assert!(perm(None, None)
            .allows_egress(None, "anthropic", Some("conn-z"))
            .is_ok());
    }

    fn connections() -> Vec<(String, String)> {
        [
            ("conn-a", "openai"),
            ("conn-b", "openai"),
            ("conn-c", "anthropic"),
        ]
        .into_iter()
        .map(|(id, kind)| (id.to_string(), kind.to_string()))
        .collect()
    }

    fn scopes_of(p: &ApiKeyPermission) -> Vec<(String, Vec<String>)> {
        p.connection_scopes
            .iter()
            .flatten()
            .map(|scope| (scope.connection_id.clone(), scope.models.clone()))
            .collect()
    }

    fn expected(entries: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
        entries
            .iter()
            .map(|(id, models)| {
                (
                    id.to_string(),
                    models.iter().map(|model| model.to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn legacy_fields_restate_as_the_same_direct_access() {
        let connections = connections();
        for (stored, restated) in [
            // provider: every connection of that kind, all models.
            (
                perm(Some("openai"), None),
                expected(&[("conn-a", &[]), ("conn-b", &[])]),
            ),
            // connection_id with models: that connection, those models.
            (
                ApiKeyPermission {
                    connection_id: Some("conn-c".into()),
                    models: Some(vec!["anthropic/claude".into(), "chalk/a".into()]),
                    ..Default::default()
                },
                expected(&[("conn-c", &["anthropic/claude"])]),
            ),
            // Flat models alone: every connection, minus the route id.
            (
                perm(None, Some(&["gpt-4o", "chalk/a"])),
                expected(&[
                    ("conn-a", &["gpt-4o"]),
                    ("conn-b", &["gpt-4o"]),
                    ("conn-c", &["gpt-4o"]),
                ]),
            ),
            // Only route ids listed: no direct model anywhere.
            (perm(None, Some(&["chalk/a"])), expected(&[])),
            // Scopes AND-ed with provider and models.
            (
                ApiKeyPermission {
                    provider: Some("openai".into()),
                    models: Some(vec!["gpt-4o".into(), "gpt-5".into()]),
                    connection_scopes: Some(vec![
                        scope("conn-a", &["gpt-4o", "o3"]),
                        scope("conn-b", &[]),
                        scope("conn-c", &[]),
                    ]),
                    ..Default::default()
                },
                expected(&[("conn-a", &["gpt-4o"]), ("conn-b", &["gpt-4o", "gpt-5"])]),
            ),
            // A dangling legacy connection stays, so the key stays restricted.
            (perm_conn("conn-gone"), expected(&[("conn-gone", &[])])),
            // No legacy field: route ids are dropped from scopes, and a scope
            // that listed only route ids goes rather than becoming "any model".
            (
                ApiKeyPermission {
                    connection_scopes: Some(vec![
                        scope("conn-a", &["chalk/a"]),
                        scope("conn-b", &["chalk/a", "gpt-4o"]),
                    ]),
                    ..Default::default()
                },
                expected(&[("conn-b", &["gpt-4o"])]),
            ),
        ] {
            let p = stored.without_legacy_fields(&connections);
            assert!(!p.has_legacy_grants(), "{stored:?}");
            assert!(p.is_restricted(), "{stored:?}");
            assert_eq!(scopes_of(&p), restated, "{stored:?}");
            for (connection, kind) in &connections {
                for model in ["gpt-4o", "gpt-5", "o3", "anthropic/claude"] {
                    assert_eq!(
                        p.allows(kind, Some(connection), model).is_ok(),
                        stored.allows(kind, Some(connection), model).is_ok(),
                        "{stored:?} on {connection} for {model}"
                    );
                }
            }
        }
        let current = ApiKeyPermission {
            connection_scopes: Some(vec![scope("conn-a", &["gpt-4o"])]),
            route_ids: Some(vec!["route-a".into()]),
            ..Default::default()
        };
        assert!(!current.has_legacy_grants());
        assert_eq!(
            scopes_of(&current.without_legacy_fields(&connections)),
            scopes_of(&current)
        );
    }

    #[test]
    fn unrestricted_allows_anything() {
        let p = perm(None, None);
        assert!(!p.is_restricted());
        assert!(p
            .allows("openai", Some("id-openai"), "openai/gpt-4o")
            .is_ok());
        assert!(p.allows("anthropic", None, "chalk/gemma").is_ok());
    }

    fn resolution(kind: &str, conn: &str, prefix: Option<&str>) -> crate::backend::RouteResolution {
        crate::backend::RouteResolution {
            provider_kind: kind.to_string(),
            connection_id: Some(conn.to_string()),
            connection_name: conn.to_string(),
            upstream_model_id: String::new(),
            connection_prefix: prefix.map(str::to_string),
            model_route_id: None,
        }
    }

    #[test]
    fn bare_model_ids_match_their_listed_form_on_the_canonical_connection() {
        // `/models` lists the `openai/` connection's models as `openai/<id>`;
        // a request may name the bare id and reach the same connection and
        // upstream model.
        let openai = resolution("openai", "id-openai", Some("openai/"));
        let p = perm(None, Some(&["openai/gpt-6-luna"]));
        assert!(p.allows_route(&openai, "gpt-6-luna").is_ok());
        assert!(p.allows_route(&openai, "openai/gpt-6-luna").is_ok());
        assert!(matches!(
            p.allows_route(&openai, "gpt-5.5"),
            Err(ApiKeyError::ModelDenied { model }) if model == "gpt-5.5"
        ));
        // A connection with a custom prefix is not what `openai/...` names,
        // even when bare ids happen to resolve to it.
        let custom = resolution("openai", "id-acme", Some("acme/"));
        assert!(matches!(
            p.allows_route(&custom, "gpt-6-luna"),
            Err(ApiKeyError::ModelDenied { .. })
        ));
        let no_prefix = resolution("openai", "id-bare", None);
        assert!(matches!(
            p.allows_route(&no_prefix, "gpt-6-luna"),
            Err(ApiKeyError::ModelDenied { .. })
        ));
        // Only the provider's own canonical prefix is implied.
        let anthropic = resolution("anthropic", "id-a", Some("anthropic/"));
        assert!(matches!(
            p.allows_route(&anthropic, "gpt-6-luna"),
            Err(ApiKeyError::ModelDenied { .. })
        ));
        // A listed bare id does not grant the prefixed alias, and `chalk/*`
        // routes never normalise.
        let p = perm(None, Some(&["gpt-6-luna"]));
        assert!(matches!(
            p.allows_route(&openai, "openai/gpt-6-luna"),
            Err(ApiKeyError::ModelDenied { .. })
        ));
        let p = perm(None, Some(&["chalk/gemma"]));
        assert!(matches!(
            p.allows_route(&openai, "gemma"),
            Err(ApiKeyError::ModelDenied { .. })
        ));
        // The same shorthand applies inside a connection scope, and the other
        // axes still apply first.
        let p = ApiKeyPermission {
            connection_scopes: Some(vec![ApiKeyConnectionScope {
                connection_id: "id-openai".to_string(),
                models: vec!["openai/gpt-6-luna".to_string()],
            }]),
            ..Default::default()
        };
        assert!(p.allows_route(&openai, "gpt-6-luna").is_ok());
        assert!(matches!(
            p.allows_route(
                &resolution("openai", "id-other", Some("openai/")),
                "gpt-6-luna"
            ),
            Err(ApiKeyError::ConnectionDenied)
        ));
        // Plain `allows` stays exact.
        let p = perm(None, Some(&["openai/gpt-6-luna"]));
        assert!(matches!(
            p.allows("openai", Some("id-openai"), "gpt-6-luna"),
            Err(ApiKeyError::ModelDenied { .. })
        ));
    }

    #[test]
    fn provider_matches_on_kind() {
        let p = perm(Some("anthropic"), None);
        assert!(p.is_restricted());
        assert!(p
            .allows("anthropic", Some("id-a"), "anthropic/claude")
            .is_ok());
        // Wrong kind -> denied, and the error names the *attempted* kind.
        match p.allows("openai", Some("id-o"), "openai/gpt-4o") {
            Err(ApiKeyError::ProviderDenied { provider }) => assert_eq!(provider, "openai"),
            other => panic!("expected ProviderDenied, got {other:?}"),
        }
    }

    #[test]
    fn provider_kind_is_not_satisfied_by_a_colliding_connection_name() {
        // P1 regression: `provider` is a *kind*, matched only against the kind.
        // An `openai-compatible` connection an operator named "openai" must NOT
        // satisfy a key restricted to the real `openai` provider — even though
        // the connection's name equals the kind string.
        let p = perm(Some("openai"), None);
        // Resolved route is an openai-compatible endpoint (its id is irrelevant).
        assert!(matches!(
            p.allows(
                "openai-compatible",
                Some("id-of-the-fake-openai"),
                "openai/gpt-4o"
            ),
            Err(ApiKeyError::ProviderDenied { .. })
        ));
        // The genuine openai kind still passes.
        assert!(p.allows("openai", Some("id-real-openai"), "gpt-4o").is_ok());
    }

    #[test]
    fn connection_id_scopes_to_one_endpoint() {
        // A key scoped to a specific endpoint keys off the connection's immutable
        // id, so it distinguishes two openai-compatible endpoints (same kind) and
        // is unaffected by a later rename.
        let p = perm_conn("conn-123");
        assert!(p.is_restricted());
        assert!(p
            .allows("openai-compatible", Some("conn-123"), "my-vllm/llama")
            .is_ok());
        // A different connection (any kind/name) is denied.
        assert!(matches!(
            p.allows("openai-compatible", Some("conn-999"), "other/llama"),
            Err(ApiKeyError::ConnectionDenied)
        ));
        // A route that resolved without a stored connection (leaf default) is
        // denied — the grant is to one specific id, so fail closed.
        assert!(matches!(
            p.allows("openai", None, "gpt-4o"),
            Err(ApiKeyError::ConnectionDenied)
        ));
    }

    #[test]
    fn models_allowlist_matches_public_id() {
        let p = perm(None, Some(&["chalk/gemma", "openai/gpt-4o"]));
        // A listed model route is not granted: only `route_ids` grants routes.
        assert!(p.allows("anthropic", None, "chalk/gemma").is_err());
        assert!(p.allows("openai", Some("id-o"), "openai/gpt-4o").is_ok());
        // A public id not on the list is denied regardless of provider.
        match p.allows("openai", Some("id-o"), "openai/gpt-3.5") {
            Err(ApiKeyError::ModelDenied { model }) => assert_eq!(model, "openai/gpt-3.5"),
            other => panic!("expected ModelDenied, got {other:?}"),
        }
    }

    #[test]
    fn all_axes_are_enforced_together() {
        let p = ApiKeyPermission {
            provider: Some("anthropic".to_string()),
            connection_id: Some("conn-a".to_string()),
            models: Some(vec!["anthropic/claude".to_string()]),
            ..Default::default()
        };
        // Every axis satisfied.
        assert!(p
            .allows("anthropic", Some("conn-a"), "anthropic/claude")
            .is_ok());
        // Provider + connection ok but model off-list -> model axis denies.
        assert!(matches!(
            p.allows("anthropic", Some("conn-a"), "anthropic/opus"),
            Err(ApiKeyError::ModelDenied { .. })
        ));
        // Right kind + model but wrong connection -> connection axis denies.
        assert!(matches!(
            p.allows("anthropic", Some("conn-b"), "anthropic/claude"),
            Err(ApiKeyError::ConnectionDenied)
        ));
        // Model on-list but provider mismatch -> provider axis denies (first).
        assert!(matches!(
            p.allows("openai", Some("conn-a"), "anthropic/claude"),
            Err(ApiKeyError::ProviderDenied { .. })
        ));
    }

    #[test]
    fn connection_scopes_pairs_each_connection_with_its_own_models() {
        let p = ApiKeyPermission {
            connection_scopes: Some(vec![
                ApiKeyConnectionScope {
                    connection_id: "conn-openai".to_string(),
                    models: vec!["openai/gpt-4o".to_string()],
                },
                ApiKeyConnectionScope {
                    connection_id: "conn-anthropic".to_string(),
                    models: vec!["anthropic/claude".to_string()],
                },
            ]),
            ..Default::default()
        };
        assert!(p
            .allows("openai", Some("conn-openai"), "openai/gpt-4o")
            .is_ok());
        assert!(p
            .allows("anthropic", Some("conn-anthropic"), "anthropic/claude")
            .is_ok());
        // The pairing is exactly the point: model x is not on connection B's
        // list even though it's on connection A's, and vice versa. A flat
        // connection list plus a shared models axis could not express this
        // — both of these would incorrectly pass under that shape.
        match p.allows("anthropic", Some("conn-anthropic"), "openai/gpt-4o") {
            Err(ApiKeyError::ModelDenied { model }) => assert_eq!(model, "openai/gpt-4o"),
            other => panic!("expected ModelDenied, got {other:?}"),
        }
        match p.allows("openai", Some("conn-openai"), "anthropic/claude") {
            Err(ApiKeyError::ModelDenied { model }) => assert_eq!(model, "anthropic/claude"),
            other => panic!("expected ModelDenied, got {other:?}"),
        }
    }

    #[test]
    fn connection_scopes_denies_a_connection_with_no_matching_entry() {
        let p = ApiKeyPermission {
            connection_scopes: Some(vec![ApiKeyConnectionScope {
                connection_id: "conn-a".to_string(),
                models: vec![],
            }]),
            ..Default::default()
        };
        assert!(matches!(
            p.allows("openai", Some("conn-b"), "openai/gpt-4o"),
            Err(ApiKeyError::ConnectionDenied)
        ));
        // No stored connection on the resolved route is denied, same as
        // connection_id: the grant is to specific ids.
        assert!(matches!(
            p.allows("openai", None, "openai/gpt-4o"),
            Err(ApiKeyError::ConnectionDenied)
        ));
    }

    #[test]
    fn connection_scopes_empty_models_list_means_any_model_on_that_connection() {
        let p = ApiKeyPermission {
            connection_scopes: Some(vec![ApiKeyConnectionScope {
                connection_id: "conn-a".to_string(),
                models: vec![],
            }]),
            ..Default::default()
        };
        assert!(p.allows("openai", Some("conn-a"), "openai/gpt-4o").is_ok());
        assert!(p.allows("openai", Some("conn-a"), "openai/gpt-3.5").is_ok());
    }

    #[test]
    fn connection_scopes_is_anded_with_every_other_axis() {
        // A key can combine connection_scopes with an unrelated axis (e.g.
        // provider); both must still pass independently.
        let p = ApiKeyPermission {
            provider: Some("openai".to_string()),
            connection_scopes: Some(vec![ApiKeyConnectionScope {
                connection_id: "conn-a".to_string(),
                models: vec!["openai/gpt-4o".to_string()],
            }]),
            ..Default::default()
        };
        assert!(p.allows("openai", Some("conn-a"), "openai/gpt-4o").is_ok());
        // Right connection/model but wrong provider kind -> provider axis
        // denies first, even though connection_scopes alone would pass.
        assert!(matches!(
            p.allows("anthropic", Some("conn-a"), "openai/gpt-4o"),
            Err(ApiKeyError::ProviderDenied { .. })
        ));
    }

    #[test]
    fn connection_scopes_gates_egress_by_connection_identity_only() {
        let p = ApiKeyPermission {
            connection_scopes: Some(vec![ApiKeyConnectionScope {
                connection_id: "conn-a".to_string(),
                models: vec!["openai/gpt-4o".to_string()],
            }]),
            ..Default::default()
        };
        // Egress only cares about connection identity, not the per-scope
        // model list — the judge model would never be on that list.
        assert!(p.allows_egress(None, "openai", Some("conn-a")).is_ok());
        assert!(matches!(
            p.allows_egress(None, "openai", Some("conn-b")),
            Err(ApiKeyError::ConnectionDenied)
        ));
    }
}

#[cfg(test)]
mod label_selection_tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_selection_matches_all_labels_and_required_grouping_key() {
        let store = InMemoryApiKeyStore::new(KeyHasher::new(b"label-selection-test".to_vec()));
        for labels in [
            HashMap::from([
                ("team".to_string(), "alpha".to_string()),
                ("feature".to_string(), "chat".to_string()),
            ]),
            HashMap::from([("team".to_string(), "alpha".to_string())]),
            HashMap::from([
                ("team".to_string(), "beta".to_string()),
                ("feature".to_string(), "batch".to_string()),
            ]),
        ] {
            store
                .create(
                    "test".to_string(),
                    ApiKeyPermission::default(),
                    labels,
                    None,
                )
                .await
                .unwrap();
        }

        let selected = store
            .select_by_labels(
                &HashMap::from([("team".to_string(), "alpha".to_string())]),
                Some("feature"),
                &[],
                10,
            )
            .await
            .unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].labels["feature"], "chat");
        assert_eq!(
            store
                .select_by_labels(&HashMap::new(), None, &[], 2)
                .await
                .unwrap()
                .len(),
            2
        );
    }
}
