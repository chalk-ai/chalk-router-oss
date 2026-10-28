use std::sync::Arc;

use crate::budget::{validate_usage_budgets, ApiKeyBudgetInput, BudgetPolicy, BudgetPolicyStore};
use crate::ratelimit::{
    normalize_rate_limits, scoped_rate_limit_policies, validate_rate_limits, ApiKeyRateLimitInput,
    RateLimitPolicyStore,
};
use crate::service::pagination::{format_cursor, parse_cursor, resolve_limit};
use crate::service::ServiceError;
use crate::usage_pools::{UsagePool, UsagePoolStore};

#[derive(Debug, Default, Clone)]
pub struct CreateUsagePoolInput {
    pub name: String,
    pub rate_limits: Vec<ApiKeyRateLimitInput>,
    pub usage_budgets: Vec<ApiKeyBudgetInput>,
}

#[derive(Debug, Default, Clone)]
pub struct UpdateUsagePoolInput {
    pub name: Option<String>,
    /// `None` leaves the set unchanged; `Some(vec![])` clears it.
    pub rate_limits: Option<Vec<ApiKeyRateLimitInput>>,
    /// `None` leaves the set unchanged; `Some(vec![])` clears it.
    pub usage_budgets: Option<Vec<ApiKeyBudgetInput>>,
}

#[derive(Debug, Clone)]
pub enum UsagePoolLookup {
    Id(String),
    Name(String),
}

pub struct UsagePoolService {
    store: Arc<dyn UsagePoolStore>,
    budget_store: Arc<dyn BudgetPolicyStore>,
    rate_limit_store: Arc<dyn RateLimitPolicyStore>,
    environment_id: String,
    quota_enabled: bool,
}

#[derive(Default)]
struct UpdateProgress {
    name: bool,
    rate_limits: bool,
    usage_budgets: bool,
}

impl UsagePoolService {
    pub fn new(
        store: Arc<dyn UsagePoolStore>,
        budget_store: Arc<dyn BudgetPolicyStore>,
        rate_limit_store: Arc<dyn RateLimitPolicyStore>,
        environment_id: String,
        quota_enabled: bool,
    ) -> Self {
        Self {
            store,
            budget_store,
            rate_limit_store,
            environment_id,
            quota_enabled,
        }
    }

    pub async fn create(&self, input: CreateUsagePoolInput) -> Result<UsagePool, ServiceError> {
        let CreateUsagePoolInput {
            name,
            mut rate_limits,
            usage_budgets,
        } = input;
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(ServiceError::invalid_field("name", "must not be empty"));
        }
        if (!rate_limits.is_empty() || !usage_budgets.is_empty()) && !self.quota_enabled {
            return Err(ServiceError::failed_precondition(
                "quota enforcement is disabled (set ROUTER_QUOTA_BACKEND)",
            ));
        }
        validate_usage_budgets(&usage_budgets)?;
        normalize_rate_limits(&mut rate_limits);
        validate_rate_limits("usage_pool", "new-pool", &rate_limits)?;

        let has_quotas = !rate_limits.is_empty() || !usage_budgets.is_empty();
        let pool = if has_quotas {
            match self
                .store
                .create_with_quota_definitions(
                    name.clone(),
                    rate_limits.clone(),
                    usage_budgets.clone(),
                )
                .await?
            {
                Some(pool) => pool,
                None => {
                    self.create_with_compensation(name, rate_limits, usage_budgets)
                        .await?
                }
            }
        } else {
            self.store.create_pool(name).await?
        };
        self.budget_store.invalidate_matching();
        self.rate_limit_store.invalidate_matching();
        tracing::info!(id = %pool.id, name = %pool.name, "Created usage pool");
        Ok(pool)
    }

    fn budget_policy(&self, pool_id: &str, budget: ApiKeyBudgetInput) -> BudgetPolicy {
        let window_size = match budget.period.as_str() {
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
        };
        BudgetPolicy {
            quota_policy_id: uuid::Uuid::new_v4().to_string(),
            id: uuid::Uuid::new_v4().to_string(),
            environment_id: self.environment_id.clone(),
            scope: "usage_pool".to_string(),
            scope_id: pool_id.to_string(),
            period: budget.period,
            window_type: "calendar".to_string(),
            window_size,
            anchor: None,
            timezone: "UTC".to_string(),
            limit_kind: "tokens".to_string(),
            limit_amount: budget.token_limit,
            enabled: budget.enabled,
            created_at: chrono::Utc::now(),
        }
    }

    fn is_usage_budget(policy: &BudgetPolicy, pool_id: &str) -> bool {
        policy.scope == "usage_pool"
            && policy.scope_id == pool_id
            && policy.limit_kind == "tokens"
            && policy.window_type == "calendar"
            && policy.timezone == "UTC"
            && matches!(policy.period.as_str(), "daily" | "weekly" | "monthly")
    }

    async fn create_with_compensation(
        &self,
        name: String,
        rate_limits: Vec<ApiKeyRateLimitInput>,
        usage_budgets: Vec<ApiKeyBudgetInput>,
    ) -> Result<UsagePool, ServiceError> {
        let pool = self.store.create_pool(name).await?;
        let mut budget_ids = Vec::new();
        let mut rate_limit_ids = Vec::new();

        for budget in usage_budgets {
            let policy = self.budget_policy(&pool.id, budget);
            let id = policy.id.clone();
            let result = self.budget_store.create(policy).await;
            match result {
                Ok(_) => budget_ids.push(id),
                Err(error) => {
                    return Err(self
                        .fail_create_after_rollback(&pool.id, &budget_ids, &rate_limit_ids, error)
                        .await)
                }
            }
        }

        for policy in scoped_rate_limit_policies("usage_pool", &pool.id, &rate_limits) {
            let id = policy.id.clone();
            match self.rate_limit_store.create(policy).await {
                Ok(_) => rate_limit_ids.push(id),
                Err(error) => {
                    return Err(self
                        .fail_create_after_rollback(&pool.id, &budget_ids, &rate_limit_ids, error)
                        .await)
                }
            }
        }
        Ok(pool)
    }

    async fn fail_create_after_rollback(
        &self,
        pool_id: &str,
        budget_ids: &[String],
        rate_limit_ids: &[String],
        original: ServiceError,
    ) -> ServiceError {
        let mut failures = Vec::new();
        if let Err(error) = self.store.delete_pool(pool_id).await {
            failures.push(format!("pool deletion: {error}"));
        } else {
            self.budget_store.invalidate_matching();
            self.rate_limit_store.invalidate_matching();
        }
        for id in budget_ids {
            match self.budget_store.delete(id).await {
                Ok(()) | Err(ServiceError::NotFound { .. }) => {}
                Err(error) => failures.push(format!("budget cleanup: {error}")),
            }
        }
        for id in rate_limit_ids {
            if let Err(error) = self.rate_limit_store.delete(id).await {
                failures.push(format!("rate-limit cleanup: {error}"));
            }
        }
        if failures.is_empty() {
            original
        } else {
            ServiceError::internal(format!(
                "usage pool provisioning failed: {original}; rollback incomplete ({})",
                failures.join("; ")
            ))
        }
    }

    pub async fn update(
        &self,
        id: &str,
        input: UpdateUsagePoolInput,
    ) -> Result<UsagePool, ServiceError> {
        let UpdateUsagePoolInput {
            name,
            mut rate_limits,
            usage_budgets,
        } = input;
        let name = name
            .map(|name| {
                let name = name.trim().to_string();
                if name.is_empty() {
                    Err(ServiceError::invalid_field("name", "must not be empty"))
                } else {
                    Ok(name)
                }
            })
            .transpose()?;
        if let Some(budgets) = usage_budgets.as_ref() {
            validate_usage_budgets(budgets)?;
        }
        if let Some(limits) = rate_limits.as_mut() {
            normalize_rate_limits(limits);
            validate_rate_limits("usage_pool", "new-pool", limits)?;
        }
        let enables_quotas = usage_budgets
            .as_ref()
            .is_some_and(|items| !items.is_empty())
            || rate_limits.as_ref().is_some_and(|items| !items.is_empty());
        if enables_quotas && !self.quota_enabled {
            return Err(ServiceError::failed_precondition(
                "quota enforcement is disabled (set ROUTER_QUOTA_BACKEND)",
            ));
        }

        let quotas_changed = usage_budgets.is_some() || rate_limits.is_some();
        if quotas_changed {
            match self
                .store
                .update_with_quota_definitions(
                    id,
                    name.clone(),
                    rate_limits.clone(),
                    usage_budgets.clone(),
                )
                .await?
            {
                Some(Some(pool)) => {
                    self.budget_store.invalidate_matching();
                    self.rate_limit_store.invalidate_matching();
                    return Ok(pool);
                }
                Some(None) => return Err(ServiceError::not_found(format!("Usage pool '{id}'"))),
                None => {}
            }
        }

        let existing = self
            .store
            .get_pool(id)
            .await?
            .ok_or_else(|| ServiceError::not_found(format!("Usage pool '{id}'")))?;
        if !quotas_changed {
            return match name {
                Some(name) => self
                    .store
                    .update_pool(id, name)
                    .await?
                    .ok_or_else(|| ServiceError::not_found(format!("Usage pool '{id}'"))),
                None => Ok(existing),
            };
        }
        self.update_with_compensation(existing, name, rate_limits, usage_budgets)
            .await
    }

    async fn update_with_compensation(
        &self,
        existing: UsagePool,
        name: Option<String>,
        rate_limits: Option<Vec<ApiKeyRateLimitInput>>,
        usage_budgets: Option<Vec<ApiKeyBudgetInput>>,
    ) -> Result<UsagePool, ServiceError> {
        let original_budgets = self
            .budget_store
            .list()
            .await?
            .into_iter()
            .filter(|policy| Self::is_usage_budget(policy, &existing.id))
            .collect::<Vec<_>>();
        let original_rate_limits = self
            .rate_limit_store
            .list()
            .await
            .into_iter()
            .filter(|policy| policy.scope == "usage_pool" && policy.scope_id == existing.id)
            .collect::<Vec<_>>();

        // A rejected rename has made no changes and must not trigger quota
        // deletion/recreation during compensation (that can reset live state).
        let mut progress = UpdateProgress::default();
        let mut updated = existing.clone();
        if let Some(name) = name {
            updated = self
                .store
                .update_pool(&existing.id, name)
                .await?
                .ok_or_else(|| ServiceError::not_found(format!("Usage pool '{}'", existing.id)))?;
            progress.name = true;
        }
        let result = self
            .apply_non_atomic_update(
                updated,
                rate_limits,
                usage_budgets,
                &original_budgets,
                &original_rate_limits,
                &mut progress,
            )
            .await;
        match result {
            Ok(pool) => Ok(pool),
            Err(original) => {
                let rollback = self
                    .rollback_failed_update(
                        &existing,
                        progress.name,
                        progress.rate_limits,
                        progress.usage_budgets,
                        &original_budgets,
                        &original_rate_limits,
                    )
                    .await;
                match rollback {
                    Ok(()) => Err(original),
                    Err(rollback) => Err(ServiceError::internal(format!(
                        "usage pool update failed: {original}; rollback incomplete ({rollback})"
                    ))),
                }
            }
        }
    }

    async fn apply_non_atomic_update(
        &self,
        updated: UsagePool,
        rate_limits: Option<Vec<ApiKeyRateLimitInput>>,
        usage_budgets: Option<Vec<ApiKeyBudgetInput>>,
        original_budgets: &[BudgetPolicy],
        original_rate_limits: &[crate::ratelimit::RateLimitPolicy],
        progress: &mut UpdateProgress,
    ) -> Result<UsagePool, ServiceError> {
        if let Some(budgets) = usage_budgets {
            progress.usage_budgets = true;
            for policy in original_budgets {
                self.budget_store.delete(&policy.id).await?;
            }
            for budget in budgets {
                self.budget_store
                    .create(self.budget_policy(&updated.id, budget))
                    .await?;
            }
        }
        if let Some(limits) = rate_limits {
            progress.rate_limits = true;
            for policy in original_rate_limits {
                self.rate_limit_store.delete(&policy.id).await?;
            }
            for policy in scoped_rate_limit_policies("usage_pool", &updated.id, &limits) {
                self.rate_limit_store.create(policy).await?;
            }
        }
        Ok(updated)
    }

    async fn rollback_failed_update(
        &self,
        existing: &UsagePool,
        restore_name: bool,
        restore_rate_limits: bool,
        restore_usage_budgets: bool,
        original_budgets: &[BudgetPolicy],
        original_rate_limits: &[crate::ratelimit::RateLimitPolicy],
    ) -> Result<(), ServiceError> {
        let mut failures = Vec::new();
        if restore_name {
            if let Err(error) = self
                .store
                .update_pool(&existing.id, existing.name.clone())
                .await
            {
                failures.push(format!("name restore: {error}"));
            }
        }
        if restore_usage_budgets {
            match self.budget_store.list().await {
                Ok(policies) => {
                    for policy in policies
                        .into_iter()
                        .filter(|policy| Self::is_usage_budget(policy, &existing.id))
                    {
                        if let Err(error) = self.budget_store.delete(&policy.id).await {
                            failures.push(format!("budget cleanup: {error}"));
                        }
                    }
                }
                Err(error) => failures.push(format!("budget discovery: {error}")),
            }
        }
        if restore_rate_limits {
            for policy in self
                .rate_limit_store
                .list()
                .await
                .into_iter()
                .filter(|policy| policy.scope == "usage_pool" && policy.scope_id == existing.id)
            {
                if let Err(error) = self.rate_limit_store.delete(&policy.id).await {
                    failures.push(format!("rate-limit cleanup: {error}"));
                }
            }
        }
        if restore_usage_budgets {
            for policy in original_budgets {
                if let Err(error) = self.budget_store.create(policy.clone()).await {
                    failures.push(format!("budget restore: {error}"));
                }
            }
        }
        if restore_rate_limits {
            for policy in original_rate_limits {
                if let Err(error) = self.rate_limit_store.create(policy.clone()).await {
                    failures.push(format!("rate-limit restore: {error}"));
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(ServiceError::internal(failures.join("; ")))
        }
    }

    pub async fn list(&self) -> Vec<UsagePool> {
        self.store.list_pools().await
    }

    /// Cursor-paginated variant of [`UsagePoolService::list`]. `limit`
    /// defaults to 40 (rejecting <= 0, clamping above 1000); `cursor`
    /// defaults to now(). Returns the page plus a `next_cursor` that is set
    /// iff more rows remain.
    pub async fn list_page(
        &self,
        limit: Option<i32>,
        cursor: Option<String>,
    ) -> Result<(Vec<UsagePool>, Option<String>), ServiceError> {
        let limit = resolve_limit(limit)?;
        let before = parse_cursor(cursor.as_deref())?;

        let mut page = self.store.list_pools_page(before, limit + 1).await?;
        if page.len() as i64 > limit {
            page.truncate(limit as usize);
            let next_cursor = page.last().map(|p| format_cursor(p.created_at));
            Ok((page, next_cursor))
        } else {
            Ok((page, None))
        }
    }

    pub async fn lookup(&self, lookup: Option<UsagePoolLookup>) -> Result<UsagePool, ServiceError> {
        match lookup {
            Some(UsagePoolLookup::Id(id)) => self.get(&id).await,
            Some(UsagePoolLookup::Name(name)) => {
                let name = name.trim();
                if name.is_empty() {
                    return Err(ServiceError::invalid_field("name", "must not be empty"));
                }
                self.store
                    .get_pool_by_name(name)
                    .await?
                    .ok_or_else(|| ServiceError::not_found(format!("Usage pool named '{name}'")))
            }
            None => Err(ServiceError::invalid_field(
                "lookup",
                "must specify id or name",
            )),
        }
    }

    /// Point lookup by id.
    pub async fn get(&self, id: &str) -> Result<UsagePool, ServiceError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(ServiceError::invalid_field("id", "must not be empty"));
        }
        self.store
            .get_pool(id)
            .await?
            .ok_or_else(|| ServiceError::not_found(format!("Usage pool '{id}'")))
    }

    /// Idempotent: deleting an unknown or already-removed pool is success.
    pub async fn delete(&self, id: &str) -> Result<(), ServiceError> {
        let budget_ids = self
            .budget_store
            .list()
            .await?
            .into_iter()
            .filter(|policy| policy.scope == "usage_pool" && policy.scope_id == id)
            .map(|policy| policy.id)
            .collect::<Vec<_>>();
        let rate_limit_ids = self
            .rate_limit_store
            .list()
            .await
            .into_iter()
            .filter(|policy| policy.scope == "usage_pool" && policy.scope_id == id)
            .map(|policy| policy.id)
            .collect::<Vec<_>>();
        let _ = self.store.delete_pool(id).await?;
        self.budget_store.invalidate_matching();
        self.rate_limit_store.invalidate_matching();
        for budget_id in budget_ids {
            // Co-located stores already removed these definitions in the pool
            // transaction. Non-co-located stores still need compensation.
            match self.budget_store.delete(&budget_id).await {
                Ok(()) | Err(ServiceError::NotFound { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        for rate_limit_id in rate_limit_ids {
            self.rate_limit_store.delete(&rate_limit_id).await?;
        }
        Ok(())
    }
}
