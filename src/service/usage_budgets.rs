use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::api_keys::ApiKeyStore;
use crate::budget::{BudgetPolicy, BudgetPolicyStore};
use crate::quota::{FixedWindowUsageQuery, QuotaBackend};
use crate::quota_policy::period_window;
use crate::service::ServiceError;
use crate::usage_pools::UsagePoolStore;

/// Control-plane operations for canonical daily token budgets.  These are
/// enforcement definitions, rather than a historical usage ledger.
pub struct UsageBudgetService {
    budget_store: Arc<dyn BudgetPolicyStore>,
    api_key_store: Arc<dyn ApiKeyStore>,
    usage_pool_store: Arc<dyn UsagePoolStore>,
    environment_id: String,
    quota_enabled: bool,
    quota_backend: Arc<dyn QuotaBackend>,
}

/// A [`BudgetPolicy`] enriched with the live quota-backend usage for its
/// current fixed window, for callers (the gRPC adapter) that need to render
/// enforcement state alongside the definition. `current_usage` is `None` when
/// the backend has no enforcement state to report (for example, disabled).
pub struct BudgetPolicyUsage {
    pub policy: BudgetPolicy,
    pub current_usage: Option<u64>,
    pub window_end: DateTime<Utc>,
}

impl UsageBudgetService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        budget_store: Arc<dyn BudgetPolicyStore>,
        api_key_store: Arc<dyn ApiKeyStore>,
        usage_pool_store: Arc<dyn UsagePoolStore>,
        environment_id: String,
        quota_enabled: bool,
        quota_backend: Arc<dyn QuotaBackend>,
    ) -> Self {
        Self {
            budget_store,
            api_key_store,
            usage_pool_store,
            environment_id,
            quota_enabled,
            quota_backend,
        }
    }

    /// Enrich budget policies with their live quota-backend usage for the
    /// current fixed window. Moved from the gRPC adapter's former
    /// `with_runtime` helper so that quota-backend access — like every other
    /// enforcement concern — lives in the service layer, not the adapter.
    ///
    /// The counter is intentionally returned as enforcement state: until a
    /// request settles, its conservative reservation is included. It must
    /// not be mistaken for an exact historical usage ledger.
    pub async fn with_usage(
        &self,
        policies: Vec<BudgetPolicy>,
    ) -> Result<Vec<BudgetPolicyUsage>, ServiceError> {
        let now = Utc::now();
        let windows = policies
            .iter()
            .map(|policy| period_window(now, &policy.period))
            .collect::<Result<Vec<_>, ServiceError>>()?;
        let queries = policies
            .iter()
            .zip(&windows)
            .map(|(policy, (window_start, _))| FixedWindowUsageQuery {
                policy_id: policy.quota_policy_id.clone(),
                window_start: *window_start,
            })
            .collect::<Vec<_>>();
        let usage = self
            .quota_backend
            .fixed_window_usage(&queries)
            .await
            .map_err(|error| {
                ServiceError::internal(format!("quota usage lookup failed: {error}"))
            })?;
        Ok(policies
            .into_iter()
            .zip(windows)
            .map(|(policy, (_, window_end))| {
                let current_usage = usage
                    .as_ref()
                    .and_then(|values| values.get(&policy.quota_policy_id).copied());
                BudgetPolicyUsage {
                    policy,
                    current_usage,
                    window_end,
                }
            })
            .collect())
    }

    pub async fn create(
        &self,
        scope: String,
        scope_id: String,
        token_limit: u64,
        period: String,
        enabled: bool,
    ) -> Result<BudgetPolicy, ServiceError> {
        self.validate_definition(&scope, &scope_id, token_limit)
            .await?;
        let period = Self::normalize_period(&period)?;
        if self.budget_store.list().await?.into_iter().any(|budget| {
            budget.scope == scope && budget.scope_id == scope_id && budget.period == period
        }) {
            return Err(ServiceError::invalid_field(
                "scope_id",
                "already has a token budget for this period; update that budget instead",
            ));
        }
        self.budget_store
            .create(BudgetPolicy {
                quota_policy_id: uuid::Uuid::new_v4().to_string(),
                id: uuid::Uuid::new_v4().to_string(),
                environment_id: self.environment_id.clone(),
                scope,
                scope_id,
                period: period.clone(),
                window_type: "calendar".into(),
                window_size: match period.as_str() {
                    "weekly" => sqlx::postgres::types::PgInterval {
                        months: 0,
                        days: 7,
                        microseconds: 0,
                    },
                    "monthly" => sqlx::postgres::types::PgInterval {
                        months: 1,
                        days: 0,
                        microseconds: 0,
                    },
                    _ => sqlx::postgres::types::PgInterval {
                        months: 0,
                        days: 1,
                        microseconds: 0,
                    },
                },
                anchor: None,
                timezone: "UTC".into(),
                limit_kind: "tokens".into(),
                limit_amount: token_limit,
                enabled,
                created_at: chrono::Utc::now(),
            })
            .await
    }

    pub async fn list(
        &self,
        scope: Option<String>,
        scope_id: Option<String>,
    ) -> Result<Vec<BudgetPolicy>, ServiceError> {
        Ok(self
            .budget_store
            .list()
            .await?
            .into_iter()
            .filter(|policy| {
                Self::is_canonical(policy)
                    && scope.as_ref().is_none_or(|scope| policy.scope == *scope)
                    && scope_id
                        .as_ref()
                        .is_none_or(|scope_id| policy.scope_id == *scope_id)
            })
            .collect())
    }

    pub async fn get(&self, id: &str) -> Result<BudgetPolicy, ServiceError> {
        self.budget_store
            .get(id)
            .await?
            .filter(Self::is_canonical)
            .ok_or_else(|| ServiceError::not_found(format!("Usage budget '{id}'")))
    }

    pub async fn update(
        &self,
        id: &str,
        token_limit: Option<u64>,
        enabled: Option<bool>,
    ) -> Result<BudgetPolicy, ServiceError> {
        // Same gate `create` enforces via `validate_definition`: budgets are
        // an enforcement definition, not a passive record, so neither side
        // of the CRUD surface should let one through while quota
        // enforcement is off.
        if !self.quota_enabled {
            return Err(ServiceError::failed_precondition(
                "quota enforcement is disabled (set ROUTER_QUOTA_BACKEND)",
            ));
        }
        if let Some(amount) = token_limit {
            Self::validate_amount(amount)?;
        }
        let existing = self
            .budget_store
            .get(id)
            .await?
            .ok_or_else(|| ServiceError::not_found(format!("Usage budget '{id}'")))?;
        if !Self::is_canonical(&existing) {
            return Err(ServiceError::not_found(format!("Usage budget '{id}'")));
        }
        self.budget_store.update(id, token_limit, enabled).await
    }

    pub async fn delete(&self, id: &str) -> Result<(), ServiceError> {
        let policy = self
            .budget_store
            .get(id)
            .await?
            .ok_or_else(|| ServiceError::not_found(format!("Usage budget '{id}'")))?;
        if !Self::is_canonical(&policy) {
            return Err(ServiceError::not_found(format!("Usage budget '{id}'")));
        }
        self.budget_store.delete(id).await
    }

    fn is_canonical(policy: &BudgetPolicy) -> bool {
        policy.window_type == "calendar"
            && matches!(policy.period.as_str(), "daily" | "weekly" | "monthly")
            && policy.limit_kind == "tokens"
            && policy.timezone == "UTC"
    }

    fn normalize_period(period: &str) -> Result<String, ServiceError> {
        match period.to_ascii_lowercase().as_str() {
            "daily" => Ok("daily".into()),
            "weekly" => Ok("weekly".into()),
            "monthly" => Ok("monthly".into()),
            _ => Err(ServiceError::invalid_field(
                "period",
                "must be DAILY, WEEKLY, or MONTHLY",
            )),
        }
    }

    fn validate_amount(amount: u64) -> Result<(), ServiceError> {
        if amount == 0 {
            return Err(ServiceError::invalid_field(
                "daily_token_budget",
                "must be positive",
            ));
        }
        if amount > i64::MAX as u64 {
            return Err(ServiceError::invalid_field(
                "daily_token_budget",
                "must fit PostgreSQL BIGINT",
            ));
        }
        Ok(())
    }

    async fn validate_definition(
        &self,
        scope: &str,
        scope_id: &str,
        amount: u64,
    ) -> Result<(), ServiceError> {
        if !self.quota_enabled {
            return Err(ServiceError::failed_precondition(
                "quota enforcement is disabled (set ROUTER_QUOTA_BACKEND)",
            ));
        }
        Self::validate_amount(amount)?;
        if scope_id.trim().is_empty() {
            return Err(ServiceError::invalid_field("scope_id", "must not be empty"));
        }
        match scope {
            "api_key" => {
                if self.api_key_store.get(scope_id).await.is_none() {
                    return Err(ServiceError::invalid_field(
                        "scope_id",
                        "API key does not exist in this environment",
                    ));
                }
            }
            "usage_pool" => {
                if self.usage_pool_store.get_pool(scope_id).await?.is_none() {
                    return Err(ServiceError::invalid_field(
                        "scope_id",
                        "usage pool does not exist in this environment",
                    ));
                }
            }
            _ => {
                return Err(ServiceError::invalid_field(
                    "scope",
                    "must be api_key or usage_pool",
                ))
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_keys::{ApiKey, ApiKeyPermission, InMemoryApiKeyStore};
    use crate::budget::MemoryBudgetPolicyStore;
    use crate::hashing::KeyHasher;
    use crate::usage_pools::InMemoryUsagePoolStore;
    use std::collections::HashMap;

    fn service() -> UsageBudgetService {
        UsageBudgetService::new(
            Arc::new(MemoryBudgetPolicyStore::new("env-a".into())),
            Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(b"test".to_vec()))),
            Arc::new(InMemoryUsagePoolStore::new()),
            "env-a".into(),
            true,
            Arc::new(crate::quota::DisabledQuota),
        )
    }

    #[tokio::test]
    async fn rejects_missing_and_duplicate_scope_targets() {
        let service = service();
        assert!(matches!(
            service
                .create("api_key".into(), "missing".into(), 1, "daily".into(), true)
                .await,
            Err(ServiceError::InvalidArgument { .. })
        ));
    }

    #[tokio::test]
    async fn update_rejected_when_quota_disabled() {
        // `update` now carries the same gate `create` always has: budgets
        // are an enforcement definition, so neither side of the CRUD
        // surface should succeed while quota enforcement is off.
        let budget_store = Arc::new(MemoryBudgetPolicyStore::new("env-a".into()));
        let key_store = Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(b"test".to_vec())));
        key_store
            .insert_raw(ApiKey {
                key_hash: "key".into(),
                key_prefix: "key".into(),
                description: "key".into(),
                created_at: chrono::Utc::now(),
                permission: ApiKeyPermission::default(),
                labels: HashMap::new(),
                usage_pool_id: None,
                daily_token_budget: None,
            })
            .await
            .unwrap();
        let usage_pool_store = Arc::new(InMemoryUsagePoolStore::new());
        let enabled_service = UsageBudgetService::new(
            budget_store.clone(),
            key_store.clone(),
            usage_pool_store.clone(),
            "env-a".into(),
            true,
            Arc::new(crate::quota::DisabledQuota),
        );
        let created = enabled_service
            .create("api_key".into(), "key".into(), 10, "daily".into(), true)
            .await
            .expect("create on the enabled backend succeeds");

        let disabled_service = UsageBudgetService::new(
            budget_store,
            key_store,
            usage_pool_store,
            "env-a".into(),
            false,
            Arc::new(crate::quota::DisabledQuota),
        );
        let err = disabled_service
            .update(&created.id, Some(20), None)
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::FailedPrecondition { .. }));
    }

    #[tokio::test]
    async fn rejects_duplicate_canonical_budget() {
        let budget_store = Arc::new(MemoryBudgetPolicyStore::new("env-a".into()));
        let key_store = Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(b"test".to_vec())));
        key_store
            .insert_raw(ApiKey {
                key_hash: "key".into(),
                key_prefix: "key".into(),
                description: "key".into(),
                created_at: chrono::Utc::now(),
                permission: ApiKeyPermission::default(),
                labels: HashMap::new(),
                usage_pool_id: None,
                daily_token_budget: None,
            })
            .await
            .unwrap();
        let service = UsageBudgetService::new(
            budget_store,
            key_store,
            Arc::new(InMemoryUsagePoolStore::new()),
            "env-a".into(),
            true,
            Arc::new(crate::quota::DisabledQuota),
        );
        service
            .create("api_key".into(), "key".into(), 10, "daily".into(), true)
            .await
            .unwrap();
        assert!(matches!(
            service
                .create("api_key".into(), "key".into(), 20, "daily".into(), true)
                .await,
            Err(ServiceError::InvalidArgument { .. })
        ));
        service
            .create("api_key".into(), "key".into(), 20, "weekly".into(), true)
            .await
            .expect("a different period is an independent budget");
    }
}
