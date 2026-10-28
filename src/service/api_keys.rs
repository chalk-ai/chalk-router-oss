use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::api_keys::{
    is_model_route, ApiKey, ApiKeyPermission, ApiKeyStore, CreatedApiKey, UsageSnapshot,
};
use crate::budget::{validate_usage_budgets, ApiKeyBudgetInput, BudgetPolicy, BudgetPolicyStore};
use crate::model_routes::ModelRouteStore;
use crate::provider_connections::ProviderConnectionStore;
use crate::ratelimit::{
    api_key_rate_limit_policies, group_api_key_rate_limits, normalize_rate_limits,
    validate_rate_limits, ApiKeyRateLimitInput, RateLimitPolicyStore,
};
use crate::service::pagination::{parse_stable_cursor, StablePageCursor};
use crate::service::ServiceError;
use crate::usage_tracker::{UsageHistogramRow, UsageTracker};

/// One route grant supplied by an admin client. This request-only type is
/// resolved before storage; permissions used by the data plane contain only ids.
#[derive(Debug, Clone)]
pub enum ApiKeyRouteRef {
    RouteId(String),
    PublicModelId(String),
}

/// A permission write, including route names that must be resolved before use.
/// Construct from a canonical permission, then replace `routes` to supply names.
#[derive(Debug, Default, Clone)]
pub struct ApiKeyPermissionInput {
    policy: ApiKeyPermission,
    pub routes: Option<Vec<ApiKeyRouteRef>>,
}

impl From<ApiKeyPermission> for ApiKeyPermissionInput {
    fn from(mut policy: ApiKeyPermission) -> Self {
        let routes = policy
            .route_ids
            .take()
            .map(|ids| ids.into_iter().map(ApiKeyRouteRef::RouteId).collect());
        Self { policy, routes }
    }
}

/// Inputs for [`ApiKeyService::create`]. Optional fields default per [`Default`].
#[derive(Debug, Default, Clone)]
pub struct CreateKeyInput {
    pub description: Option<String>,
    pub permission: Option<ApiKeyPermissionInput>,
    pub labels: Option<HashMap<String, String>>,
    pub daily_token_budget: Option<u64>,
    pub cost_tags: Option<HashMap<String, String>>,
    pub usage_pool_id: Option<String>,
    pub rate_limits: Vec<ApiKeyRateLimitInput>,
    /// Complete direct key budget set. The legacy daily field cannot be used
    /// alongside this canonical representation.
    pub usage_budgets: Option<Vec<ApiKeyBudgetInput>>,
}

/// Inputs for [`ApiKeyService::update`].
///
/// `usage_pool_id` uses `Option<Option<T>>` to distinguish "do not touch this
/// field" (None) from "clear this field" (Some(None)).
#[derive(Debug, Default, Clone)]
pub struct UpdateKeyInput {
    pub description: Option<String>,
    /// Full replacement for the access, judge, and cache policy. `None` leaves
    /// the existing permission unchanged; `Some(default())` removes all
    /// restrictions and restores inherited policy.
    pub permission: Option<ApiKeyPermissionInput>,
    pub cost_tags: Option<HashMap<String, String>>,
    pub daily_token_budget: Option<Option<u64>>,
    pub usage_pool_id: Option<Option<String>>,
    /// `None` leaves direct API-key rate limits unchanged; `Some` replaces the
    /// complete direct set, with `Some(vec![])` removing it.
    pub rate_limits: Option<Vec<ApiKeyRateLimitInput>>,
    /// `None` leaves direct budgets unchanged; `Some(vec![])` removes them.
    pub usage_budgets: Option<Vec<ApiKeyBudgetInput>>,
}

#[derive(Debug)]
pub struct KeyUsageView {
    /// Key identifier (the `key_hash`); never the raw secret.
    pub key: String,
    pub description: String,
    pub daily_token_budget: Option<u64>,
    pub usage: UsageSnapshot,
}

#[derive(Debug)]
pub struct ApiKeyWithUsage {
    pub api_key: Arc<ApiKey>,
    pub usage: UsageSnapshot,
    pub rate_limits: Vec<ApiKeyRateLimitInput>,
    pub usage_budgets: Vec<BudgetPolicy>,
}

#[derive(Debug, Default, Clone)]
pub struct UsageHistogramInput {
    pub bucket_size: Option<String>,
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub api_token: Option<String>,
}

pub struct ApiKeyService {
    store: Arc<dyn ApiKeyStore>,
    usage_tracker: Option<UsageTracker>,
    budget: Arc<dyn BudgetPolicyStore>,
    rate_limits: Arc<dyn RateLimitPolicyStore>,
    /// Read to restate legacy fields for admin reads.
    connections: Arc<dyn ProviderConnectionStore>,
    /// Read to validate route grants.
    routes: Arc<dyn ModelRouteStore>,
    quota_enabled: bool,
}

const MAX_DESCRIPTION_CHARS: usize = 120;

/// Reject the legacy fields, which the admin API no longer writes.
fn reject_legacy_fields(permission: &ApiKeyPermission) -> Result<(), ServiceError> {
    for (field, set) in [
        ("permission.provider", permission.provider.is_some()),
        (
            "permission.connection_id",
            permission.connection_id.is_some(),
        ),
        ("permission.models", permission.models.is_some()),
    ] {
        if set {
            return Err(ServiceError::invalid_field(
                field,
                "this field is no longer accepted; grant direct models with \
                 permission.connection_scopes and model routes with permission.routes",
            ));
        }
    }
    Ok(())
}

/// Connection-scope model lists hold exact public ids of direct models. A
/// pattern such as `openai/*` used to be stored and echoed back while matching
/// nothing, and a `chalk/*` id never grants its route here.
fn validate_connection_scopes(permission: &ApiKeyPermission) -> Result<(), ServiceError> {
    for (index, scope) in permission.connection_scopes.iter().flatten().enumerate() {
        let field = format!("permission.connection_scopes[{index}].models");
        for model in &scope.models {
            if model.contains('*') {
                return Err(ServiceError::invalid_field(
                    field,
                    format!(
                        "model '{model}' contains a wildcard; model lists match exact public \
                         model ids. Leave models empty to allow every model on the connection"
                    ),
                ));
            }
            if is_model_route(model) {
                return Err(ServiceError::invalid_field(
                    field,
                    format!("'{model}' is a model route; grant it with permission.routes instead"),
                ));
            }
        }
    }
    Ok(())
}

fn validate_description(description: String) -> Result<String, ServiceError> {
    let description = description.trim();
    if description.is_empty() {
        return Err(ServiceError::invalid_field(
            "description",
            "description is required",
        ));
    }
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(ServiceError::invalid_field(
            "description",
            format!("description must be {MAX_DESCRIPTION_CHARS} characters or fewer"),
        ));
    }
    if description.chars().any(|character| character.is_control()) {
        return Err(ServiceError::invalid_field(
            "description",
            "description must not contain control characters",
        ));
    }
    Ok(description.to_owned())
}

impl ApiKeyService {
    pub fn new(
        store: Arc<dyn ApiKeyStore>,
        usage_tracker: Option<UsageTracker>,
        budget: Arc<dyn BudgetPolicyStore>,
        rate_limits: Arc<dyn RateLimitPolicyStore>,
        connections: Arc<dyn ProviderConnectionStore>,
        routes: Arc<dyn ModelRouteStore>,
        quota_enabled: bool,
    ) -> Self {
        Self {
            store,
            usage_tracker,
            budget,
            rate_limits,
            connections,
            routes,
            quota_enabled,
        }
    }

    /// Validate a permission written through the admin API and normalize its
    /// route grants. `existing` is the key's current permission on update:
    /// route ids it already carries are kept even if their route has since
    /// been deleted, so an unrelated edit does not fail on them.
    async fn validate_permission(
        &self,
        input: ApiKeyPermissionInput,
        existing: Option<&ApiKeyPermission>,
    ) -> Result<ApiKeyPermission, ServiceError> {
        let ApiKeyPermissionInput {
            mut policy,
            routes: refs,
        } = input;
        reject_legacy_fields(&policy)?;
        validate_connection_scopes(&policy)?;
        if let Some(refs) = refs {
            let kept = existing
                .and_then(|existing| existing.route_ids.as_deref())
                .unwrap_or_default();
            // One environment-scoped snapshot resolves both identifier kinds and
            // avoids a separate lookup for every grant in a large permission.
            let needs_lookup = refs.iter().any(|identifier| match identifier {
                ApiKeyRouteRef::RouteId(id) => !kept.contains(id),
                ApiKeyRouteRef::PublicModelId(_) => true,
            });
            let routes = if needs_lookup {
                self.routes.list().await.map_err(|error| {
                    tracing::error!(%error, "Failed to list model routes");
                    ServiceError::internal("failed to read model routes")
                })?
            } else {
                Vec::new()
            };
            let mut seen = std::collections::HashSet::new();
            let mut ids = Vec::new();
            for (index, identifier) in refs.into_iter().enumerate() {
                let (field, value, id) = match &identifier {
                    ApiKeyRouteRef::RouteId(id) => (
                        "route_id",
                        id,
                        (kept.contains(id) || routes.iter().any(|route| &route.id == id))
                            .then_some(id),
                    ),
                    ApiKeyRouteRef::PublicModelId(public_id) => (
                        "public_model_id",
                        public_id,
                        routes
                            .iter()
                            .find(|route| &route.public_model_id == public_id)
                            .map(|route| &route.id),
                    ),
                };
                let path = format!("permission.routes.refs[{index}].{field}");
                if value.trim().is_empty() || value.contains('*') {
                    return Err(ServiceError::invalid_field(
                        &path,
                        format!("{path}: must be a non-empty exact route identifier"),
                    ));
                }
                let id = id.ok_or_else(|| {
                    ServiceError::invalid_field(
                        &path,
                        format!("{path}: model route '{value}' does not exist"),
                    )
                })?;
                if seen.insert(id.clone()) {
                    ids.push(id.clone());
                }
            }
            policy.route_ids = Some(ids);
        }
        Ok(policy)
    }

    /// Present keys as the admin API returns them, with legacy fields
    /// restated as connection scopes (see
    /// [`ApiKeyPermission::without_legacy_fields`]).
    async fn present(&self, keys: Vec<Arc<ApiKey>>) -> Result<Vec<Arc<ApiKey>>, ServiceError> {
        if !keys.iter().any(|key| key.permission.has_legacy_grants()) {
            return Ok(keys);
        }
        let connections: Vec<(String, String)> =
            if keys.iter().any(|key| key.permission.has_legacy_fields()) {
                self.connections
                    .list_metadata()
                    .await
                    .map_err(|error| {
                        tracing::error!(%error, "Failed to list provider connections");
                        ServiceError::internal("failed to read provider connections")
                    })?
                    .into_iter()
                    .map(|connection| (connection.id, connection.provider_kind))
                    .collect()
            } else {
                Vec::new()
            };
        Ok(keys
            .into_iter()
            .map(|key| {
                if !key.permission.has_legacy_grants() {
                    return key;
                }
                Arc::new(ApiKey {
                    key_hash: key.key_hash.clone(),
                    key_prefix: key.key_prefix.clone(),
                    description: key.description.clone(),
                    created_at: key.created_at,
                    permission: key.permission.without_legacy_fields(&connections),
                    labels: key.labels.clone(),
                    usage_pool_id: key.usage_pool_id.clone(),
                    daily_token_budget: key.daily_token_budget,
                })
            })
            .collect())
    }

    /// Reject enabling a definition when no runtime can enforce it.
    fn ensure_budget_writable(&self, requested: bool) -> Result<(), ServiceError> {
        if requested && !self.quota_enabled {
            return Err(ServiceError::failed_precondition(
                "quota enforcement is disabled (set ROUTER_QUOTA_BACKEND)",
            ));
        }
        Ok(())
    }

    fn validate_budget(amount: Option<u64>) -> Result<(), ServiceError> {
        if amount == Some(0) {
            return Err(ServiceError::invalid_field(
                "daily_token_budget",
                "must be positive",
            ));
        }
        if amount.is_some_and(|amount| amount > i64::MAX as u64) {
            return Err(ServiceError::invalid_field(
                "daily_token_budget",
                "must fit PostgreSQL BIGINT",
            ));
        }
        Ok(())
    }

    fn validate_usage_budgets(&self, budgets: &[ApiKeyBudgetInput]) -> Result<(), ServiceError> {
        if !budgets.is_empty() && !self.quota_enabled {
            return Err(ServiceError::failed_precondition(
                "quota enforcement is disabled (set ROUTER_QUOTA_BACKEND)",
            ));
        }
        validate_usage_budgets(budgets)
    }

    fn validate_rate_limits(
        &self,
        permission: &ApiKeyPermission,
        limits: &[ApiKeyRateLimitInput],
    ) -> Result<(), ServiceError> {
        if !limits.is_empty() && !self.quota_enabled {
            return Err(ServiceError::failed_precondition(
                "quota enforcement is disabled (set ROUTER_QUOTA_BACKEND)",
            ));
        }
        validate_rate_limits("api_key", "direct-key-draft", limits)?;

        // Permission-scoping checks have no counterpart in usage pools (which
        // have no per-key permission set), so they stay local rather than
        // moving into the shared `crate::ratelimit` validator. They cover
        // direct models only: route traffic may reach any connection its
        // route uses, so a limit on a granted route's traffic is not checked
        // against the connection scopes.
        let routes_granted = permission
            .route_ids
            .as_ref()
            .is_some_and(|ids| !ids.is_empty());
        for (index, limit) in limits.iter().enumerate() {
            if let Some(connection_id) = limit.connection_id.as_ref() {
                let direct: Vec<&String> = limit
                    .models
                    .iter()
                    .filter(|model| !is_model_route(model))
                    .collect();
                if (!limit.models.is_empty() && direct.is_empty())
                    || (limit.models.is_empty() && routes_granted)
                {
                    continue;
                }
                if permission
                    .connection_id
                    .as_ref()
                    .is_some_and(|connection| connection != connection_id)
                {
                    return Err(ServiceError::invalid_field(
                        "rate_limits",
                        format!("rate_limits[{index}] targets a connection the key cannot access"),
                    ));
                }
                if let Some(models) = &permission.models {
                    if models.is_empty()
                        || direct.is_empty()
                        || direct
                            .iter()
                            .any(|model| !models.iter().any(|allowed| allowed == *model))
                    {
                        return Err(ServiceError::invalid_field(
                            "rate_limits",
                            format!("rate_limits[{index}] targets a model the key cannot access"),
                        ));
                    }
                }
                if let Some(scopes) = &permission.connection_scopes {
                    let Some(scope) = scopes
                        .iter()
                        .find(|scope| scope.connection_id == connection_id.as_str())
                    else {
                        return Err(ServiceError::invalid_field(
                            "rate_limits",
                            format!(
                                "rate_limits[{index}] targets a connection the key cannot access"
                            ),
                        ));
                    };
                    if !scope.models.is_empty()
                        && (direct.is_empty()
                            || direct
                                .iter()
                                .any(|model| !scope.models.iter().any(|allowed| allowed == *model)))
                    {
                        return Err(ServiceError::invalid_field(
                            "rate_limits",
                            format!("rate_limits[{index}] targets a model the key cannot access"),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn normalize_rate_limits(mut limits: Vec<ApiKeyRateLimitInput>) -> Vec<ApiKeyRateLimitInput> {
        normalize_rate_limits(&mut limits);
        limits
    }

    async fn rollback_failed_create(&self, key_hash: &str) -> Result<(), ServiceError> {
        let mut failures = Vec::new();

        // Remove the credential first. Definition cleanup can be retried, but
        // a key must not remain usable without the policies requested for it.
        if let Err(error) = self.store.revoke(key_hash).await {
            tracing::error!(key_hash = %key_hash, %error, "Failed to revoke API key during create rollback");
            failures.push(format!("key revoke: {error}"));
        } else {
            self.budget.invalidate_matching();
            self.rate_limits.invalidate_matching();
        }

        let (rate_limits, budget) = tokio::join!(
            self.rate_limits.delete_for_api_key(key_hash),
            self.budget.replace_key_budgets(key_hash, Vec::new()),
        );
        if let Err(error) = rate_limits {
            tracing::error!(key_hash = %key_hash, %error, "Failed to clear rate limits during create rollback");
            failures.push(format!("rate-limit cleanup: {error}"));
        }
        if let Err(error) = budget {
            tracing::error!(key_hash = %key_hash, %error, "Failed to clear budget during create rollback");
            failures.push(format!("budget cleanup: {error}"));
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(ServiceError::internal(format!(
                "API key create rollback was incomplete ({})",
                failures.join("; ")
            )))
        }
    }

    async fn fail_create_after_rollback(
        &self,
        key_hash: &str,
        original: ServiceError,
    ) -> ServiceError {
        match self.rollback_failed_create(key_hash).await {
            Ok(()) => original,
            Err(rollback) => ServiceError::internal(format!(
                "API key provisioning failed: {original}; {rollback}"
            )),
        }
    }

    /// Create a key. Returns the raw secret exactly once (see [`CreatedApiKey`]).
    pub async fn create(&self, input: CreateKeyInput) -> Result<CreatedApiKey, ServiceError> {
        let CreateKeyInput {
            description,
            permission,
            labels,
            daily_token_budget,
            cost_tags,
            usage_pool_id,
            rate_limits,
            usage_budgets,
        } = input;
        if daily_token_budget.is_some() && usage_budgets.is_some() {
            return Err(ServiceError::invalid_field(
                "usage_budgets",
                "cannot combine deprecated daily_token_budget with usage_budgets",
            ));
        }
        self.ensure_budget_writable(daily_token_budget.is_some())?;
        Self::validate_budget(daily_token_budget)?;
        if let Some(budgets) = &usage_budgets {
            self.validate_usage_budgets(budgets)?;
        }
        let description = validate_description(description.ok_or_else(|| {
            ServiceError::invalid_field("description", "description is required")
        })?)?;
        let permission = self
            .validate_permission(permission.unwrap_or_default(), None)
            .await?;
        let rate_limits = Self::normalize_rate_limits(rate_limits);
        self.validate_rate_limits(&permission, &rate_limits)?;
        let labels = labels.unwrap_or_default();
        if cost_tags.is_some() {
            return Err(ServiceError::invalid_field(
                "cost_tags",
                "cost_tags is deprecated; use immutable labels when creating the key",
            ));
        }
        crate::labels::validate(&labels)
            .map_err(|error| ServiceError::invalid_field("labels", error.to_string()))?;
        let atomic_budget = (daily_token_budget.is_some() || usage_budgets.is_some())
            && self.budget.co_locates_key_writes();
        let atomic_rate_limits =
            !rate_limits.is_empty() && self.rate_limits.co_locates_key_writes();
        let (created, budget_written_atomically, rate_limits_written_atomically) =
            if atomic_budget || atomic_rate_limits {
                match self
                    .store
                    .create_with_quota_definitions(
                        description.clone(),
                        permission.clone(),
                        labels.clone(),
                        usage_pool_id.clone(),
                        if atomic_budget {
                            daily_token_budget
                        } else {
                            None
                        },
                        if atomic_rate_limits {
                            rate_limits.clone()
                        } else {
                            Vec::new()
                        },
                        if atomic_budget {
                            usage_budgets.clone()
                        } else {
                            None
                        },
                    )
                    .await?
                {
                    Some(created) => (created, atomic_budget, atomic_rate_limits),
                    None => {
                        let created = self
                            .store
                            .create(description, permission, labels, usage_pool_id)
                            .await?;
                        (created, false, false)
                    }
                }
            } else {
                let created = self
                    .store
                    .create(description, permission, labels, usage_pool_id)
                    .await?;
                (created, false, false)
            };

        if budget_written_atomically {
            self.budget.invalidate_matching();
        }
        if rate_limits_written_atomically {
            self.rate_limits.invalidate_matching();
        }
        let key_hash = &created.api_key.key_hash;
        if daily_token_budget.is_some() && !budget_written_atomically {
            if let Err(error) = self
                .budget
                .set_key_daily_budget(key_hash, daily_token_budget)
                .await
            {
                return Err(self.fail_create_after_rollback(key_hash, error).await);
            }
        }
        if !rate_limits_written_atomically {
            let policies = api_key_rate_limit_policies(key_hash, &rate_limits);
            for policy in policies {
                if let Err(error) = self.rate_limits.create(policy).await {
                    return Err(self.fail_create_after_rollback(key_hash, error).await);
                }
            }
        }
        if let Some(budgets) = usage_budgets.filter(|_| !budget_written_atomically) {
            if let Err(error) = self.budget.replace_key_budgets(key_hash, budgets).await {
                return Err(self.fail_create_after_rollback(key_hash, error).await);
            }
        }
        tracing::info!(key_prefix = %created.api_key.key_prefix, "Created new API key");
        Ok(created)
    }

    pub async fn list(&self) -> Vec<Arc<ApiKey>> {
        self.store.list().await
    }

    /// Exhaustive, non-paginated key list enriched with one batched
    /// usage-ledger read. The gRPC path uses [`Self::list_page_with_usage`]
    /// instead.
    pub async fn list_with_usage(&self) -> Result<Vec<ApiKeyWithUsage>, ServiceError> {
        let keys = self.store.list().await;
        self.keys_with_usage(keys).await
    }

    /// Cursor-paginated key listing for the admin RPC. Cursors are opaque and
    /// carry both ordering fields so page boundaries neither repeat nor skip
    /// keys created in the same timestamp tick.
    pub async fn list_page(
        &self,
        limit: Option<i32>,
        cursor: Option<String>,
    ) -> Result<(Vec<Arc<ApiKey>>, Option<String>), ServiceError> {
        let limit = crate::service::pagination::resolve_limit(limit)?;
        let cursor = parse_stable_cursor(cursor.as_deref())?;
        let mut keys = self.store.list_page(cursor.as_ref(), limit + 1).await?;
        if keys.len() as i64 > limit {
            keys.truncate(limit as usize);
            let next_cursor = keys.last().map(|key| {
                StablePageCursor {
                    created_at: key.created_at,
                    id: key.key_hash.clone(),
                }
                .encode()
            });
            Ok((keys, next_cursor))
        } else {
            Ok((keys, None))
        }
    }

    pub async fn list_page_with_usage(
        &self,
        limit: Option<i32>,
        cursor: Option<String>,
    ) -> Result<(Vec<ApiKeyWithUsage>, Option<String>), ServiceError> {
        let (keys, next_cursor) = self.list_page(limit, cursor).await?;
        Ok((self.keys_with_usage(keys).await?, next_cursor))
    }

    pub async fn get(&self, key_hash: &str) -> Result<Arc<ApiKey>, ServiceError> {
        self.store
            .get(key_hash)
            .await
            .ok_or_else(|| ServiceError::not_found(format!("Key '{key_hash}'")))
    }

    pub async fn get_with_usage(&self, key_hash: &str) -> Result<ApiKeyWithUsage, ServiceError> {
        let key = self.get(key_hash).await?;
        self.keys_with_usage(vec![key])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| ServiceError::not_found(format!("Key '{key_hash}'")))
    }

    pub async fn update(
        &self,
        key_hash: &str,
        input: UpdateKeyInput,
    ) -> Result<Arc<ApiKey>, ServiceError> {
        let UpdateKeyInput {
            description,
            permission,
            cost_tags,
            daily_token_budget,
            usage_pool_id,
            rate_limits,
            usage_budgets,
        } = input;
        if cost_tags.is_some() {
            return Err(ServiceError::invalid_field(
                "cost_tags",
                "cost_tags is deprecated and API-key labels are immutable",
            ));
        }
        if daily_token_budget.is_some() && usage_budgets.is_some() {
            return Err(ServiceError::invalid_field(
                "usage_budgets",
                "cannot combine deprecated daily_token_budget with usage_budgets",
            ));
        }
        self.ensure_budget_writable(matches!(daily_token_budget, Some(Some(_))))?;
        Self::validate_budget(daily_token_budget.flatten())?;
        if let Some(budgets) = &usage_budgets {
            self.validate_usage_budgets(budgets)?;
        }
        let description = description.map(validate_description).transpose()?;
        let existing = self.get(key_hash).await?;
        let permission = match permission {
            Some(permission) => Some(
                self.validate_permission(permission, Some(&existing.permission))
                    .await?,
            ),
            None => None,
        };
        let rate_limits = rate_limits.map(Self::normalize_rate_limits);
        if let Some(rate_limits) = &rate_limits {
            self.validate_rate_limits(
                permission.as_ref().unwrap_or(&existing.permission),
                rate_limits,
            )?;
        }
        // Preserve the set/clear distinction: None = don't touch, Some(None) =
        // clear, Some(Some(s)) = set. The id flows through as a bare string.
        let quotas_changed =
            daily_token_budget.is_some() || rate_limits.is_some() || usage_budgets.is_some();
        let (updated, quotas_written_atomically) = if quotas_changed
            && self.budget.co_locates_key_writes()
            && self.rate_limits.co_locates_key_writes()
        {
            match self
                .store
                .update_with_quota_definitions(
                    key_hash,
                    description.clone(),
                    usage_pool_id.clone(),
                    permission.clone(),
                    daily_token_budget,
                    rate_limits.clone(),
                    usage_budgets.clone(),
                )
                .await?
            {
                Some(updated) => (updated, true),
                None => (
                    self.store
                        .update(key_hash, description, usage_pool_id, permission)
                        .await?,
                    false,
                ),
            }
        } else {
            (
                self.store
                    .update(key_hash, description, usage_pool_id, permission)
                    .await?,
                false,
            )
        };
        match updated {
            Some(k) => {
                // The API-key transaction bypasses the definition-store wrappers.
                self.budget.invalidate_matching();
                self.rate_limits.invalidate_matching();
                // Persist the budget change (Some(Some) sets, Some(None) clears).
                if !quotas_written_atomically {
                    if let Some(budget) = daily_token_budget {
                        self.budget.set_key_daily_budget(key_hash, budget).await?;
                    }
                    if let Some(rate_limits) = rate_limits {
                        self.rate_limits.delete_for_api_key(key_hash).await?;
                        for policy in api_key_rate_limit_policies(key_hash, &rate_limits) {
                            self.rate_limits.create(policy).await?;
                        }
                    }
                }
                if let Some(budgets) = usage_budgets.filter(|_| !quotas_written_atomically) {
                    self.budget.replace_key_budgets(key_hash, budgets).await?;
                }
                tracing::info!(key_hash = %key_hash, "Updated API key");
                Ok(k)
            }
            None => Err(ServiceError::not_found(format!("Key '{key_hash}'"))),
        }
    }

    /// Idempotent: revoking an unknown or already-revoked key is success.
    pub async fn revoke(&self, key_hash: &str) -> Result<(), ServiceError> {
        let co_located_budget = self.budget.co_locates_key_writes();
        let co_located_rate_limits = self.rate_limits.co_locates_key_writes();
        let revoked = if co_located_budget || co_located_rate_limits {
            match self.store.revoke_with_quota_definitions(key_hash).await? {
                Some(revoked) => revoked,
                None => self.store.revoke(key_hash).await?,
            }
        } else {
            self.store.revoke(key_hash).await?
        };
        if revoked {
            self.budget.invalidate_matching();
            self.rate_limits.invalidate_matching();
            let mut failures = Vec::new();
            if !co_located_budget {
                if let Err(e) = self.budget.replace_key_budgets(key_hash, Vec::new()).await {
                    tracing::error!(key_hash = %key_hash, error = %e, "Failed to clear budget on revoke");
                    failures.push(format!("budget cleanup: {e}"));
                }
            }
            if !co_located_rate_limits {
                if let Err(e) = self.rate_limits.delete_for_api_key(key_hash).await {
                    tracing::error!(key_hash = %key_hash, error = %e, "Failed to clear rate limits on revoke");
                    failures.push(format!("rate-limit cleanup: {e}"));
                }
            }
            if !failures.is_empty() {
                return Err(ServiceError::internal(format!(
                    "API key was revoked but quota cleanup was incomplete ({})",
                    failures.join("; ")
                )));
            }
            tracing::info!(key_hash = %key_hash, "Revoked API key");
        }
        Ok(())
    }

    pub async fn get_usage(&self, key_hash: &str) -> Result<KeyUsageView, ServiceError> {
        // These are independent reads; run them concurrently instead of paying
        // three sequential round trips.
        let key_hashes = [key_hash.to_string()];
        let (api_key, daily_token_budget, usage) = tokio::join!(
            self.get(key_hash),
            self.budget.get_key_daily_budget(key_hash),
            self.usage_snapshots(&key_hashes),
        );
        let api_key = api_key?;
        let daily_token_budget = daily_token_budget.unwrap_or(None);
        let mut usage = usage?;
        Ok(KeyUsageView {
            key: api_key.key_hash.clone(),
            description: api_key.description.clone(),
            daily_token_budget,
            usage: usage.remove(key_hash).unwrap_or_default(),
        })
    }

    pub async fn list_usage(&self) -> Result<Vec<KeyUsageView>, ServiceError> {
        let keys = self.store.list().await;
        self.usage_views(keys).await
    }

    async fn usage_views(&self, keys: Vec<Arc<ApiKey>>) -> Result<Vec<KeyUsageView>, ServiceError> {
        // Fetch every key's budget and usage in two parallel batched reads
        // instead of issuing an N+1 query for either value.
        let key_hashes: Vec<String> = keys.iter().map(|k| k.key_hash.clone()).collect();
        let (budgets, usage) = tokio::join!(
            self.budget.get_key_daily_budgets(&key_hashes),
            self.usage_snapshots(&key_hashes),
        );
        let budgets = budgets.unwrap_or_default();
        let mut usage = usage?;
        Ok(keys
            .into_iter()
            .map(|k| {
                let daily_token_budget = budgets.get(&k.key_hash).copied();
                let snapshot = usage.remove(&k.key_hash).unwrap_or_default();
                KeyUsageView {
                    key: k.key_hash.clone(),
                    description: k.description.clone(),
                    daily_token_budget,
                    usage: snapshot,
                }
            })
            .collect())
    }

    async fn keys_with_usage(
        &self,
        keys: Vec<Arc<ApiKey>>,
    ) -> Result<Vec<ApiKeyWithUsage>, ServiceError> {
        let keys = self.present(keys).await?;
        let key_hashes: Vec<String> = keys.iter().map(|key| key.key_hash.clone()).collect();
        let (usage, policies, budgets) = tokio::join!(
            self.usage_snapshots(&key_hashes),
            async { self.rate_limits.list().await },
            self.budget.list(),
        );
        let mut usage = usage?;
        let budgets = budgets?;
        let mut direct_limits: HashMap<String, Vec<_>> = HashMap::new();
        for policy in policies
            .into_iter()
            .filter(|policy| policy.scope == "api_key")
        {
            direct_limits
                .entry(policy.scope_id.clone())
                .or_default()
                .push(policy);
        }
        Ok(keys
            .into_iter()
            .map(|api_key| {
                let snapshot = usage.remove(&api_key.key_hash).unwrap_or_default();
                let rate_limits = group_api_key_rate_limits(
                    direct_limits.remove(&api_key.key_hash).unwrap_or_default(),
                );
                let usage_budgets = budgets
                    .iter()
                    .filter(|budget| {
                        budget.scope == "api_key"
                            && budget.scope_id == api_key.key_hash
                            && budget.limit_kind == "tokens"
                            && budget.window_type == "calendar"
                            && budget.timezone == "UTC"
                            && matches!(budget.period.as_str(), "daily" | "weekly" | "monthly")
                    })
                    .cloned()
                    .collect();
                ApiKeyWithUsage {
                    api_key,
                    usage: snapshot,
                    rate_limits,
                    usage_budgets,
                }
            })
            .collect())
    }

    async fn usage_snapshots(
        &self,
        key_hashes: &[String],
    ) -> Result<HashMap<String, UsageSnapshot>, ServiceError> {
        let Some(tracker) = self.usage_tracker.as_ref() else {
            return Ok(HashMap::new());
        };
        tracker.snapshots(key_hashes).await.map_err(|error| {
            tracing::error!(%error, "Failed to query API key usage snapshots");
            ServiceError::internal("Failed to query usage data")
        })
    }

    pub async fn usage_histogram(
        &self,
        input: UsageHistogramInput,
    ) -> Result<Vec<UsageHistogramRow>, ServiceError> {
        let tracker = self.usage_tracker.as_ref().ok_or_else(|| {
            ServiceError::failed_precondition(
                "Usage tracking is disabled (set ROUTER_USAGE_BACKEND)",
            )
        })?;

        let bucket_size = input.bucket_size.as_deref().unwrap_or("1h");
        let end = input.end.unwrap_or_else(Utc::now);
        let start = input.start.unwrap_or(end - chrono::Duration::hours(24));

        tracker
            .histogram(input.api_token.as_deref(), start, end, bucket_size)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "Failed to query usage histogram");
                ServiceError::internal("Failed to query usage data")
            })
    }
}
