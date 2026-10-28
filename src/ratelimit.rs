//! Rate-limit policy definitions.
//!
//! This module owns the complete public control-plane rows. Mutable counters
//! live in [`crate::quota`].

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::service::ServiceError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitPolicy {
    /// Stable internal bridge identity; not exposed through the admin API.
    #[serde(default)]
    pub quota_policy_id: String,
    pub id: String,
    pub limit_type: String,
    pub limit_value: u64,
    /// Maximum immediately spendable capacity. Concurrency policies leave it
    /// absent because `limit_value` is already the active-request cap.
    pub burst_value: Option<u64>,
    /// `None` means a global limit acquired once per client request.
    pub connection_id: Option<String>,
    /// Public model IDs. Empty means every model on the selected connection.
    /// Global limits must leave this empty.
    pub models: Vec<String>,
    /// `api_key` or `usage_pool`.
    pub scope: String,
    pub scope_id: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

pub const RATE_LIMIT_IDENTITY_CONFLICT: &str =
    "This target already has this rate limit. Edit the existing rate limit instead.";

/// Canonical identity for a rate-limit definition. Mutable values such as the
/// limit, burst, and enabled flag deliberately do not participate.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RateLimitIdentity {
    pub scope: String,
    pub scope_id: String,
    pub limit_type: String,
    pub connection_id: Option<String>,
    pub models: Vec<String>,
}

impl RateLimitIdentity {
    pub fn new(
        scope: &str,
        scope_id: &str,
        limit_type: &str,
        connection_id: Option<&str>,
        models: &[String],
    ) -> Self {
        let connection_id = connection_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let mut models = models
            .iter()
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty())
            .collect::<Vec<_>>();
        models.sort();
        models.dedup();
        if connection_id.is_none() {
            models.clear();
        }
        Self {
            scope: scope.trim().to_string(),
            scope_id: scope_id.trim().to_string(),
            limit_type: limit_type.trim().to_string(),
            connection_id,
            models,
        }
    }
}

pub fn rate_limit_identity(policy: &RateLimitPolicy) -> RateLimitIdentity {
    RateLimitIdentity::new(
        &policy.scope,
        &policy.scope_id,
        &policy.limit_type,
        policy.connection_id.as_deref(),
        &policy.models,
    )
}

pub fn normalize_rate_limit_identity_fields(policy: &mut RateLimitPolicy) {
    let identity = rate_limit_identity(policy);
    policy.scope = identity.scope;
    policy.scope_id = identity.scope_id;
    policy.limit_type = identity.limit_type;
    policy.connection_id = identity.connection_id;
    policy.models = identity.models;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiKeyRateLimitInput {
    pub connection_id: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
    pub requests_per_minute: Option<u64>,
    pub request_burst: Option<u64>,
    pub tokens_per_minute: Option<u64>,
    pub token_burst: Option<u64>,
    pub concurrent_requests: Option<u64>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

/// Trim, drop empty, sort, and dedup the free-form `connection_id`/`models`
/// fields shared by every rate-limit editor (direct API-key limits and
/// usage-pool limits). Mutates in place.
pub(crate) fn normalize_rate_limits(limits: &mut [ApiKeyRateLimitInput]) {
    for limit in limits {
        limit.connection_id = limit
            .connection_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        limit.models = limit
            .models
            .drain(..)
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty())
            .collect();
        limit.models.sort();
        limit.models.dedup();
    }
}

/// Rate-limit model lists match exact public model ids, never wildcard patterns.
/// Callers supply the field path so nested and standalone editors report the
/// selector that needs correcting.
pub(crate) fn validate_rate_limit_models(
    models: &[String],
    field: &str,
) -> Result<(), ServiceError> {
    if let Some(model) = models.iter().find(|model| model.contains('*')) {
        return Err(ServiceError::invalid_field(
            field,
            format!(
                "{field} entry '{model}' contains a wildcard; model lists match exact public model ids \
                 (omit models to cover the whole connection)"
            ),
        ));
    }
    Ok(())
}

/// Structural validation shared by every rate-limit editor: at least one
/// configured limit, positive/PostgreSQL-BIGINT-bounded values, burst values
/// requiring their base rate, models requiring a connection, and no two
/// limits in the batch resolving to the same identity. `scope`/`scope_id`
/// scope the identity conflict check the same way a persisted policy would
/// (see [`RateLimitIdentity::new`]); callers that have not yet allocated a
/// real scope_id may pass a stable placeholder, since the check only needs
/// to catch conflicts within this batch. Callers are responsible for any
/// precondition checks (such as whether quota enforcement is enabled at
/// all) and for any additional scoping checks (such as an API key's
/// permission grants) before or after calling this.
pub(crate) fn validate_rate_limits(
    scope: &str,
    scope_id: &str,
    limits: &[ApiKeyRateLimitInput],
) -> Result<(), ServiceError> {
    let mut identities = std::collections::HashSet::new();
    for (index, limit) in limits.iter().enumerate() {
        if limit.connection_id.is_none() && !limit.models.is_empty() {
            return Err(ServiceError::invalid_field(
                "rate_limits",
                format!("rate_limits[{index}].models requires connection_id"),
            ));
        }
        validate_rate_limit_models(&limit.models, &format!("rate_limits[{index}].models"))?;
        if limit.requests_per_minute.is_none()
            && limit.tokens_per_minute.is_none()
            && limit.concurrent_requests.is_none()
        {
            return Err(ServiceError::invalid_field(
                "rate_limits",
                format!("rate_limits[{index}] must configure at least one limit"),
            ));
        }
        for (field, value) in [
            ("requests_per_minute", limit.requests_per_minute),
            ("request_burst", limit.request_burst),
            ("tokens_per_minute", limit.tokens_per_minute),
            ("token_burst", limit.token_burst),
            ("concurrent_requests", limit.concurrent_requests),
        ] {
            if value == Some(0) {
                return Err(ServiceError::invalid_field(
                    "rate_limits",
                    format!("rate_limits[{index}].{field} must be positive"),
                ));
            }
            if value.is_some_and(|value| value > i64::MAX as u64) {
                return Err(ServiceError::invalid_field(
                    "rate_limits",
                    format!("rate_limits[{index}].{field} exceeds PostgreSQL BIGINT"),
                ));
            }
        }
        if limit.request_burst.is_some() && limit.requests_per_minute.is_none() {
            return Err(ServiceError::invalid_field(
                "rate_limits",
                format!("rate_limits[{index}].request_burst requires requests_per_minute"),
            ));
        }
        if limit.token_burst.is_some() && limit.tokens_per_minute.is_none() {
            return Err(ServiceError::invalid_field(
                "rate_limits",
                format!("rate_limits[{index}].token_burst requires tokens_per_minute"),
            ));
        }
        for (limit_type, value) in [
            ("requests_per_minute", limit.requests_per_minute),
            ("tokens_per_minute", limit.tokens_per_minute),
            ("concurrent_requests", limit.concurrent_requests),
        ] {
            if value.is_some()
                && !identities.insert(RateLimitIdentity::new(
                    scope,
                    scope_id,
                    limit_type,
                    limit.connection_id.as_deref(),
                    &limit.models,
                ))
            {
                return Err(ServiceError::invalid_field(
                    "rate_limits",
                    RATE_LIMIT_IDENTITY_CONFLICT,
                ));
            }
        }
    }
    Ok(())
}

pub fn api_key_rate_limit_policies(
    key_hash: &str,
    limits: &[ApiKeyRateLimitInput],
) -> Vec<RateLimitPolicy> {
    scoped_rate_limit_policies("api_key", key_hash, limits)
}

pub fn scoped_rate_limit_policies(
    scope: &str,
    scope_id: &str,
    limits: &[ApiKeyRateLimitInput],
) -> Vec<RateLimitPolicy> {
    let now = Utc::now();
    let mut policies = Vec::new();
    for limit in limits {
        let dimensions = [
            (
                "requests_per_minute",
                limit.requests_per_minute,
                limit.request_burst,
            ),
            (
                "tokens_per_minute",
                limit.tokens_per_minute,
                limit.token_burst,
            ),
            ("concurrent_requests", limit.concurrent_requests, None),
        ];
        for (kind, value, burst) in dimensions {
            let Some(value) = value else { continue };
            let mut policy = RateLimitPolicy {
                quota_policy_id: uuid::Uuid::new_v4().to_string(),
                id: uuid::Uuid::new_v4().to_string(),
                limit_type: kind.to_string(),
                limit_value: value,
                burst_value: (kind != "concurrent_requests").then_some(burst.unwrap_or(value)),
                connection_id: limit.connection_id.clone(),
                models: limit.models.clone(),
                scope: scope.to_string(),
                scope_id: scope_id.to_string(),
                enabled: limit.enabled,
                created_at: now,
            };
            normalize_rate_limit_identity_fields(&mut policy);
            policies.push(policy);
        }
    }
    policies
}

/// Fold persisted direct API-key policy rows back into the compact form used
/// by key create/update. Usage-pool policies are deliberately excluded by the
/// caller because they are inherited, not owned by the key editor.
pub fn group_api_key_rate_limits(
    policies: impl IntoIterator<Item = RateLimitPolicy>,
) -> Vec<ApiKeyRateLimitInput> {
    let mut grouped: std::collections::BTreeMap<
        (Option<String>, Vec<String>, bool),
        ApiKeyRateLimitInput,
    > = std::collections::BTreeMap::new();
    for policy in policies {
        let entry = grouped
            .entry((
                policy.connection_id.clone(),
                policy.models.clone(),
                policy.enabled,
            ))
            .or_insert_with(|| ApiKeyRateLimitInput {
                connection_id: policy.connection_id.clone(),
                models: policy.models.clone(),
                requests_per_minute: None,
                request_burst: None,
                tokens_per_minute: None,
                token_burst: None,
                concurrent_requests: None,
                enabled: policy.enabled,
            });
        match policy.limit_type.as_str() {
            "requests_per_minute" => {
                entry.requests_per_minute = Some(policy.limit_value);
                entry.request_burst = policy.burst_value;
            }
            "tokens_per_minute" => {
                entry.tokens_per_minute = Some(policy.limit_value);
                entry.token_burst = policy.burst_value;
            }
            "concurrent_requests" => entry.concurrent_requests = Some(policy.limit_value),
            _ => {}
        }
    }
    grouped.into_values().collect()
}

#[derive(Debug, Clone, Copy)]
pub enum RateLimitModel<'a> {
    /// Match policies selecting this public model ID.
    Exact(&'a str),
    /// Match the public ID and its canonical bare/prefixed equivalent, only
    /// when the resolved connection owns that provider's canonical prefix.
    Resolved {
        public_model_id: &'a str,
        resolution: &'a crate::backend::RouteResolution,
    },
    /// The operation will use a provider-selected model. Return model-scoped
    /// policies so admission can fail closed and require an explicit model.
    ProviderSelected,
    /// The operation has no model dimension, so only connection-wide policies
    /// apply (for example, retrieving a stored response).
    NotApplicable,
}

impl RateLimitModel<'_> {
    /// Preserve exact custom/chalk aliases. Bare shorthand is equivalent only
    /// on the canonical prefix owner and for the same upstream model.
    pub(crate) fn selector_ids(self) -> Vec<String> {
        match self {
            Self::Exact(id) => vec![id.to_string()],
            Self::Resolved {
                public_model_id,
                resolution,
            } => {
                let mut ids = vec![public_model_id.to_string()];
                if let Some(prefix) =
                    crate::provider_registry::canonical_prefix(&resolution.provider_kind)
                {
                    if resolution.connection_prefix.as_deref() == Some(prefix) {
                        let bare = public_model_id
                            .strip_prefix(prefix)
                            .unwrap_or(public_model_id);
                        if !bare.is_empty()
                            && !bare.contains('/')
                            && bare == resolution.upstream_model_id
                        {
                            let alias = if bare == public_model_id {
                                format!("{prefix}{bare}")
                            } else {
                                bare.to_string()
                            };
                            ids.push(alias);
                        }
                    }
                }
                ids
            }
            Self::ProviderSelected | Self::NotApplicable => Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RateLimitMatch<'a> {
    pub key_hash: &'a str,
    pub usage_pool_id: Option<&'a str>,
    /// Absent for request-level global limits and present for one upstream
    /// dispatch.
    pub connection_id: Option<&'a str>,
    /// Describes whether the operation selected a public model, delegated that
    /// choice to the provider, or has no model dimension.
    pub model: RateLimitModel<'a>,
}

#[async_trait]
pub trait RateLimitPolicyStore: Send + Sync {
    /// Notify shared matching caches after a co-located atomic write commits.
    fn invalidate_matching(&self) {}

    async fn matching_shared(
        &self,
        request: &RateLimitMatch<'_>,
    ) -> Result<Arc<Vec<RateLimitPolicy>>, ServiceError> {
        Ok(Arc::new(self.matching(request).await?))
    }

    fn co_locates_key_writes(&self) -> bool {
        false
    }

    async fn create(&self, policy: RateLimitPolicy) -> Result<RateLimitPolicy, ServiceError>;
    async fn get(&self, id: &str) -> Result<RateLimitPolicy, ServiceError>;
    async fn list(&self) -> Vec<RateLimitPolicy>;
    async fn list_page(
        &self,
        before: DateTime<Utc>,
        fetch: i64,
    ) -> Result<Vec<RateLimitPolicy>, ServiceError>;
    async fn update(&self, policy: RateLimitPolicy) -> Result<RateLimitPolicy, ServiceError>;
    async fn delete(&self, id: &str) -> Result<(), ServiceError>;
    async fn delete_for_api_key(&self, key_hash: &str) -> Result<(), ServiceError>;
    async fn matching(
        &self,
        request: &RateLimitMatch<'_>,
    ) -> Result<Vec<RateLimitPolicy>, ServiceError>;
}

pub struct MemoryRateLimitPolicyStore {
    policies: Mutex<HashMap<String, RateLimitPolicy>>,
}

impl MemoryRateLimitPolicyStore {
    pub fn new(_environment_id: String) -> Self {
        Self {
            policies: Mutex::new(HashMap::new()),
        }
    }

    fn to_pg_precision(ts: DateTime<Utc>) -> DateTime<Utc> {
        DateTime::from_timestamp(ts.timestamp(), ts.timestamp_subsec_micros() * 1_000).unwrap_or(ts)
    }

    fn sorted(&self) -> Vec<RateLimitPolicy> {
        let mut policies: Vec<_> = self
            .policies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        policies.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        policies
    }
}

#[async_trait]
impl RateLimitPolicyStore for MemoryRateLimitPolicyStore {
    async fn create(&self, mut policy: RateLimitPolicy) -> Result<RateLimitPolicy, ServiceError> {
        if policy.quota_policy_id.is_empty() {
            policy.quota_policy_id = uuid::Uuid::new_v4().to_string();
        }
        normalize_rate_limit_identity_fields(&mut policy);
        policy.created_at = Self::to_pg_precision(policy.created_at);
        let identity = rate_limit_identity(&policy);
        let mut policies = self
            .policies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if policies.contains_key(&policy.id) {
            return Err(ServiceError::internal(format!(
                "rate-limit policy '{}' already exists",
                policy.id
            )));
        }
        if policies
            .values()
            .any(|existing| rate_limit_identity(existing) == identity)
        {
            return Err(ServiceError::invalid_argument(RATE_LIMIT_IDENTITY_CONFLICT));
        }
        policies.insert(policy.id.clone(), policy.clone());
        Ok(policy)
    }

    async fn list(&self) -> Vec<RateLimitPolicy> {
        self.sorted()
    }

    async fn get(&self, id: &str) -> Result<RateLimitPolicy, ServiceError> {
        self.policies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| ServiceError::not_found(format!("Rate-limit policy '{id}'")))
    }

    async fn list_page(
        &self,
        before: DateTime<Utc>,
        fetch: i64,
    ) -> Result<Vec<RateLimitPolicy>, ServiceError> {
        let mut page: Vec<_> = self
            .sorted()
            .into_iter()
            .filter(|policy| policy.created_at < before)
            .collect();
        page.reverse();
        page.truncate(fetch.max(0) as usize);
        Ok(page)
    }

    async fn update(&self, mut policy: RateLimitPolicy) -> Result<RateLimitPolicy, ServiceError> {
        let mut policies = self
            .policies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let existing = policies
            .get(&policy.id)
            .ok_or_else(|| ServiceError::not_found(format!("Rate-limit policy '{}'", policy.id)))?;
        policy.scope.clone_from(&existing.scope);
        policy.scope_id.clone_from(&existing.scope_id);
        normalize_rate_limit_identity_fields(&mut policy);
        let identity = rate_limit_identity(&policy);
        if policies
            .iter()
            .any(|(id, existing)| id != &policy.id && rate_limit_identity(existing) == identity)
        {
            return Err(ServiceError::invalid_argument(RATE_LIMIT_IDENTITY_CONFLICT));
        }
        policies.insert(policy.id.clone(), policy.clone());
        Ok(policy)
    }

    async fn delete(&self, id: &str) -> Result<(), ServiceError> {
        self.policies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
        Ok(())
    }

    async fn delete_for_api_key(&self, key_hash: &str) -> Result<(), ServiceError> {
        self.policies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|_, policy| !(policy.scope == "api_key" && policy.scope_id == key_hash));
        Ok(())
    }

    async fn matching(
        &self,
        request: &RateLimitMatch<'_>,
    ) -> Result<Vec<RateLimitPolicy>, ServiceError> {
        let model_ids = request.model.selector_ids();
        Ok(self
            .policies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .filter(|policy| {
                policy.enabled
                    && policy.connection_id.as_deref() == request.connection_id
                    && (request.connection_id.is_none()
                        || policy.models.is_empty()
                        || match request.model {
                            RateLimitModel::Exact(_) | RateLimitModel::Resolved { .. } => {
                                policy.models.iter().any(|model| model_ids.contains(model))
                            }
                            RateLimitModel::ProviderSelected => true,
                            RateLimitModel::NotApplicable => false,
                        })
                    && match policy.scope.as_str() {
                        "api_key" => policy.scope_id == request.key_hash,
                        "usage_pool" => request
                            .usage_pool_id
                            .is_some_and(|pool| pool == policy.scope_id),
                        _ => false,
                    }
            })
            .cloned()
            .collect())
    }
}

pub struct PostgresRateLimitPolicyStore {
    pool: sqlx::PgPool,
    environment_id: String,
}

impl PostgresRateLimitPolicyStore {
    pub fn new(pool: sqlx::PgPool, environment_id: String) -> Self {
        Self {
            pool,
            environment_id,
        }
    }

    fn db_err(error: sqlx::Error) -> ServiceError {
        if matches!(
            crate::db_error::classify(&error),
            Some(crate::db_error::DbConstraint::Unique)
        ) {
            if let sqlx::Error::Database(database) = &error {
                if matches!(
                    database.constraint(),
                    Some(
                        "rate_limits_global_identity_unique"
                            | "rate_limits_connection_identity_unique"
                    )
                ) {
                    return ServiceError::invalid_argument(RATE_LIMIT_IDENTITY_CONFLICT);
                }
            }
        }
        tracing::error!(%error, "rate-limit policy store database error");
        ServiceError::internal("database error")
    }

    fn row_to_policy(row: &sqlx::postgres::PgRow) -> RateLimitPolicy {
        use sqlx::Row;
        RateLimitPolicy {
            quota_policy_id: row.get("quota_policy_id"),
            id: row.get("id"),
            limit_type: row.get("limit_type"),
            limit_value: row.get::<i64, _>("limit_value").max(0) as u64,
            burst_value: row
                .try_get::<Option<i64>, _>("burst_value")
                .ok()
                .flatten()
                .map(|value| value.max(0) as u64),
            connection_id: row.get("connection_id"),
            models: row.get("models"),
            scope: row.get("scope"),
            scope_id: row.get("scope_id"),
            enabled: row.get("enabled"),
            created_at: row.get("created_at"),
        }
    }

    const SELECT: &'static str = "SELECT COALESCE(policy.id, '') AS quota_policy_id, limits.id, \
                limits.limit_type, limits.limit_value, limits.scope, \
                limits.scope_id, limits.enabled, limits.created_at, \
                limits.connection_id, limits.models, limits.burst_value \
         FROM chalk_router.rate_limits AS limits \
         LEFT JOIN chalk_router.quota_policies AS policy \
           ON policy.environment_id = limits.environment_id \
          AND policy.rate_limit_id = limits.id ";
}

#[async_trait]
impl RateLimitPolicyStore for PostgresRateLimitPolicyStore {
    fn co_locates_key_writes(&self) -> bool {
        true
    }

    async fn create(&self, mut policy: RateLimitPolicy) -> Result<RateLimitPolicy, ServiceError> {
        if policy.quota_policy_id.is_empty() {
            policy.quota_policy_id = uuid::Uuid::new_v4().to_string();
        }
        normalize_rate_limit_identity_fields(&mut policy);
        let limit_value = i64::try_from(policy.limit_value).map_err(|_| {
            ServiceError::invalid_field("limit_value", "must fit PostgreSQL BIGINT")
        })?;
        let burst_value = policy
            .burst_value
            .map(|value| {
                i64::try_from(value).map_err(|_| {
                    ServiceError::invalid_field("burst_value", "must fit PostgreSQL BIGINT")
                })
            })
            .transpose()?;
        sqlx::query(
            "WITH limits AS ( \
                INSERT INTO chalk_router.rate_limits \
                    (environment_id, id, limit_type, limit_value, connection_id, \
                     models, burst_value, scope, scope_id, enabled, created_at) \
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
                RETURNING id \
             ) \
             INSERT INTO chalk_router.quota_policies \
                (id, environment_id, rate_limit_id, created_at) \
             SELECT $12, $1, id, $11 FROM limits",
        )
        .bind(&self.environment_id)
        .bind(&policy.id)
        .bind(&policy.limit_type)
        .bind(limit_value)
        .bind(&policy.connection_id)
        .bind(&policy.models)
        .bind(burst_value)
        .bind(&policy.scope)
        .bind(&policy.scope_id)
        .bind(policy.enabled)
        .bind(policy.created_at)
        .bind(&policy.quota_policy_id)
        .execute(&self.pool)
        .await
        .map_err(Self::db_err)?;
        Ok(policy)
    }

    async fn list(&self) -> Vec<RateLimitPolicy> {
        let query = format!(
            "{} WHERE limits.environment_id = $1 ORDER BY limits.created_at ASC",
            Self::SELECT
        );
        sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(&self.environment_id)
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default()
            .iter()
            .map(Self::row_to_policy)
            .collect()
    }

    async fn get(&self, id: &str) -> Result<RateLimitPolicy, ServiceError> {
        let query = format!(
            "{} WHERE limits.environment_id = $1 AND limits.id = $2",
            Self::SELECT
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(&self.environment_id)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(Self::db_err)?;
        row.map(|row| Self::row_to_policy(&row))
            .ok_or_else(|| ServiceError::not_found(format!("Rate-limit policy '{id}'")))
    }

    async fn list_page(
        &self,
        before: DateTime<Utc>,
        fetch: i64,
    ) -> Result<Vec<RateLimitPolicy>, ServiceError> {
        let query = format!(
            "{} WHERE limits.environment_id = $1 AND limits.created_at < $2 \
             ORDER BY limits.created_at DESC LIMIT $3",
            Self::SELECT
        );
        Ok(sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(&self.environment_id)
            .bind(before)
            .bind(fetch)
            .fetch_all(&self.pool)
            .await
            .map_err(Self::db_err)?
            .iter()
            .map(Self::row_to_policy)
            .collect())
    }

    async fn update(&self, mut policy: RateLimitPolicy) -> Result<RateLimitPolicy, ServiceError> {
        normalize_rate_limit_identity_fields(&mut policy);
        let limit_value = i64::try_from(policy.limit_value).map_err(|_| {
            ServiceError::invalid_field("limit_value", "must fit PostgreSQL BIGINT")
        })?;
        let burst_value = policy
            .burst_value
            .map(|value| {
                i64::try_from(value).map_err(|_| {
                    ServiceError::invalid_field("burst_value", "must fit PostgreSQL BIGINT")
                })
            })
            .transpose()?;
        let ownership = sqlx::query_as::<_, (String, String)>(
            "UPDATE chalk_router.rate_limits SET limit_type = $3, limit_value = $4, \
             burst_value = $5, connection_id = $6, models = $7, enabled = $8 \
             WHERE environment_id = $1 AND id = $2 \
             RETURNING scope, scope_id",
        )
        .bind(&self.environment_id)
        .bind(&policy.id)
        .bind(&policy.limit_type)
        .bind(limit_value)
        .bind(burst_value)
        .bind(&policy.connection_id)
        .bind(&policy.models)
        .bind(policy.enabled)
        .fetch_optional(&self.pool)
        .await
        .map_err(Self::db_err)?;
        let Some((scope, scope_id)) = ownership else {
            return Err(ServiceError::not_found(format!(
                "Rate-limit policy '{}'",
                policy.id
            )));
        };
        policy.scope = scope;
        policy.scope_id = scope_id;
        Ok(policy)
    }

    async fn delete(&self, id: &str) -> Result<(), ServiceError> {
        sqlx::query("DELETE FROM chalk_router.rate_limits WHERE environment_id = $1 AND id = $2")
            .bind(&self.environment_id)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(Self::db_err)?;
        Ok(())
    }

    async fn delete_for_api_key(&self, key_hash: &str) -> Result<(), ServiceError> {
        sqlx::query(
            "DELETE FROM chalk_router.rate_limits \
             WHERE environment_id = $1 AND scope = 'api_key' AND scope_id = $2",
        )
        .bind(&self.environment_id)
        .bind(key_hash)
        .execute(&self.pool)
        .await
        .map_err(Self::db_err)?;
        Ok(())
    }

    async fn matching(
        &self,
        request: &RateLimitMatch<'_>,
    ) -> Result<Vec<RateLimitPolicy>, ServiceError> {
        let query = format!(
            "{} WHERE limits.environment_id = $1 AND limits.enabled \
               AND limits.connection_id IS NOT DISTINCT FROM $2::text \
               AND ($2::text IS NULL OR cardinality(limits.models) = 0 \
                    OR limits.models && $3::text[] OR $6::boolean) \
               AND ((limits.scope = 'api_key' AND limits.scope_id = $4) \
                 OR (limits.scope = 'usage_pool' AND $5::text IS NOT NULL \
                     AND limits.scope_id = $5)) \
             ORDER BY policy.id",
            Self::SELECT
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(&self.environment_id)
            .bind(request.connection_id)
            .bind(request.model.selector_ids())
            .bind(request.key_hash)
            .bind(request.usage_pool_id)
            .bind(matches!(request.model, RateLimitModel::ProviderSelected))
            .fetch_all(&self.pool)
            .await
            .map_err(Self::db_err)?;
        Ok(rows.iter().map(Self::row_to_policy).collect())
    }
}

pub fn from_core_store(
    pool: Option<sqlx::PgPool>,
    environment_id: String,
) -> Arc<dyn RateLimitPolicyStore> {
    let inner: Arc<dyn RateLimitPolicyStore> = match pool {
        Some(pool) => Arc::new(PostgresRateLimitPolicyStore::new(pool, environment_id)),
        None => Arc::new(MemoryRateLimitPolicyStore::new(environment_id)),
    };
    Arc::new(crate::quota_definition_cache::CachedRateLimitPolicyStore::new(inner))
}
