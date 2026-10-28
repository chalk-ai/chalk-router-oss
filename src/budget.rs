//! Budget policy definitions.
//!
//! This module owns control-plane persistence only. Request admission is
//! performed by [`crate::quota::QuotaBackend`] after
//! [`crate::quota_policy::QuotaPolicyResolver`] compiles matching definitions.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use sqlx::postgres::types::PgInterval;
use std::collections::HashMap;
use std::sync::Arc;

use crate::service::ServiceError;

#[derive(Debug, Clone)]
pub struct BudgetPolicy {
    pub quota_policy_id: String,
    pub id: String,
    pub environment_id: String,
    pub scope: String,
    pub scope_id: String,
    pub period: String,
    pub window_type: String,
    pub window_size: PgInterval,
    pub anchor: Option<DateTime<Utc>>,
    pub timezone: String,
    pub limit_kind: String,
    pub limit_amount: u64,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

/// A complete direct API-key budget set, supplied by the key APIs.  The
/// persisted policy ids are allocated by the budget store once a key exists.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApiKeyBudgetInput {
    pub token_limit: u64,
    pub period: String,
    pub enabled: bool,
}

/// Structural validation shared by every usage-budget editor (direct API-key
/// budgets and usage-pool budgets): positive, PostgreSQL-BIGINT-bounded
/// amounts, a canonical period, and no duplicate period within the batch.
/// Callers are responsible for any precondition checks (such as whether
/// quota enforcement is enabled at all) before calling this.
pub(crate) fn validate_usage_budgets(budgets: &[ApiKeyBudgetInput]) -> Result<(), ServiceError> {
    let mut periods = std::collections::HashSet::new();
    for (index, budget) in budgets.iter().enumerate() {
        if budget.token_limit == 0 {
            return Err(ServiceError::invalid_field(
                "usage_budgets",
                format!("usage_budgets[{index}].token_limit must be positive"),
            ));
        }
        if budget.token_limit > i64::MAX as u64 {
            return Err(ServiceError::invalid_field(
                "usage_budgets",
                format!("usage_budgets[{index}].token_limit must fit PostgreSQL BIGINT"),
            ));
        }
        if !matches!(budget.period.as_str(), "daily" | "weekly" | "monthly") {
            return Err(ServiceError::invalid_field(
                "usage_budgets",
                format!("usage_budgets[{index}].period must be DAILY, WEEKLY, or MONTHLY"),
            ));
        }
        if !periods.insert(&budget.period) {
            return Err(ServiceError::invalid_field(
                "usage_budgets",
                "contains more than one budget for the same period",
            ));
        }
    }
    Ok(())
}

#[async_trait]
pub trait BudgetPolicyStore: Send + Sync {
    /// Notify shared matching caches after a co-located atomic write commits.
    fn invalidate_matching(&self) {}

    async fn matching_shared(
        &self,
        key: &str,
        pool: Option<&str>,
    ) -> Result<Arc<Vec<BudgetPolicy>>, ServiceError> {
        Ok(Arc::new(self.matching(key, pool).await?))
    }

    fn co_locates_key_writes(&self) -> bool {
        false
    }

    async fn set_key_daily_budget(
        &self,
        key_hash: &str,
        daily_token_budget: Option<u64>,
    ) -> Result<(), ServiceError>;

    /// Replace every canonical direct API-key token budget. An empty set
    /// intentionally removes all direct budgets.
    async fn replace_key_budgets(
        &self,
        key_hash: &str,
        budgets: Vec<ApiKeyBudgetInput>,
    ) -> Result<(), ServiceError>;

    async fn get_key_daily_budget(&self, key_hash: &str) -> Result<Option<u64>, ServiceError>;

    async fn get_key_daily_budgets(
        &self,
        key_hashes: &[String],
    ) -> Result<HashMap<String, u64>, ServiceError>;

    async fn matching(
        &self,
        key_hash: &str,
        usage_pool_id: Option<&str>,
    ) -> Result<Vec<BudgetPolicy>, ServiceError>;

    /// Point lookup by policy id, independent of scope or canonical shape.
    /// Callers that already know the id should prefer this over `list`.
    async fn get(&self, id: &str) -> Result<Option<BudgetPolicy>, ServiceError>;

    async fn list(&self) -> Result<Vec<BudgetPolicy>, ServiceError>;
    async fn create(&self, policy: BudgetPolicy) -> Result<BudgetPolicy, ServiceError>;
    async fn update(
        &self,
        id: &str,
        amount: Option<u64>,
        enabled: Option<bool>,
    ) -> Result<BudgetPolicy, ServiceError>;
    async fn delete(&self, id: &str) -> Result<(), ServiceError>;
}

#[derive(Default)]
pub struct MemoryBudgetPolicyStore {
    environment_id: String,
    policies: DashMap<String, BudgetPolicy>,
}

impl MemoryBudgetPolicyStore {
    pub fn new(environment_id: String) -> Self {
        Self {
            environment_id,
            policies: DashMap::new(),
        }
    }

    pub fn insert(&self, policy: BudgetPolicy) -> Result<(), ServiceError> {
        if policy.environment_id != self.environment_id {
            return Err(ServiceError::invalid_field(
                "environment_id",
                "budget belongs to a different environment",
            ));
        }
        self.policies.insert(policy.id.clone(), policy);
        Ok(())
    }
}

#[async_trait]
impl BudgetPolicyStore for MemoryBudgetPolicyStore {
    async fn set_key_daily_budget(
        &self,
        key_hash: &str,
        daily_token_budget: Option<u64>,
    ) -> Result<(), ServiceError> {
        self.policies.retain(|_, policy| {
            !(policy.scope == "api_key"
                && policy.scope_id == key_hash
                && policy.limit_kind == "tokens"
                && policy.window_type == "calendar"
                && policy.window_size.months == 0
                && policy.window_size.days == 1
                && policy.window_size.microseconds == 0)
        });
        if let Some(amount) = daily_token_budget {
            let id = uuid::Uuid::new_v4().to_string();
            self.policies.insert(
                id.clone(),
                BudgetPolicy {
                    quota_policy_id: uuid::Uuid::new_v4().to_string(),
                    id,
                    environment_id: self.environment_id.clone(),
                    scope: "api_key".to_string(),
                    scope_id: key_hash.to_string(),
                    period: "daily".to_string(),
                    window_type: "calendar".to_string(),
                    window_size: PgInterval {
                        months: 0,
                        days: 1,
                        microseconds: 0,
                    },
                    anchor: None,
                    timezone: "UTC".to_string(),
                    limit_kind: "tokens".to_string(),
                    limit_amount: amount,
                    enabled: true,
                    created_at: Utc::now(),
                },
            );
        }
        Ok(())
    }

    async fn replace_key_budgets(
        &self,
        key_hash: &str,
        budgets: Vec<ApiKeyBudgetInput>,
    ) -> Result<(), ServiceError> {
        self.policies.retain(|_, policy| {
            !(policy.scope == "api_key"
                && policy.scope_id == key_hash
                && policy.limit_kind == "tokens"
                && policy.window_type == "calendar"
                && policy.timezone == "UTC"
                && matches!(policy.period.as_str(), "daily" | "weekly" | "monthly"))
        });
        for budget in budgets {
            let (months, days) = match budget.period.as_str() {
                "weekly" => (0, 7),
                "monthly" => (1, 0),
                _ => (0, 1),
            };
            let id = uuid::Uuid::new_v4().to_string();
            self.policies.insert(
                id.clone(),
                BudgetPolicy {
                    quota_policy_id: uuid::Uuid::new_v4().to_string(),
                    id,
                    environment_id: self.environment_id.clone(),
                    scope: "api_key".to_string(),
                    scope_id: key_hash.to_string(),
                    period: budget.period,
                    window_type: "calendar".to_string(),
                    window_size: PgInterval {
                        months,
                        days,
                        microseconds: 0,
                    },
                    anchor: None,
                    timezone: "UTC".to_string(),
                    limit_kind: "tokens".to_string(),
                    limit_amount: budget.token_limit,
                    enabled: budget.enabled,
                    created_at: Utc::now(),
                },
            );
        }
        Ok(())
    }

    async fn get_key_daily_budget(&self, key_hash: &str) -> Result<Option<u64>, ServiceError> {
        Ok(self
            .policies
            .iter()
            .filter(|entry| {
                let policy = entry.value();
                policy.enabled
                    && policy.scope == "api_key"
                    && policy.scope_id == key_hash
                    && policy.limit_kind == "tokens"
                    && policy.window_type == "calendar"
                    && policy.window_size.months == 0
                    && policy.window_size.days == 1
                    && policy.window_size.microseconds == 0
            })
            .max_by_key(|entry| entry.created_at)
            .map(|entry| entry.limit_amount))
    }

    async fn get_key_daily_budgets(
        &self,
        key_hashes: &[String],
    ) -> Result<HashMap<String, u64>, ServiceError> {
        // Single pass over all policies rather than one full scan per
        // requested key (the previous approach was O(keys * policies)).
        let wanted: std::collections::HashSet<&str> =
            key_hashes.iter().map(String::as_str).collect();
        let mut latest: HashMap<String, (DateTime<Utc>, u64)> = HashMap::new();
        for entry in self.policies.iter() {
            let policy = entry.value();
            if !(policy.enabled
                && policy.scope == "api_key"
                && wanted.contains(policy.scope_id.as_str())
                && policy.limit_kind == "tokens"
                && policy.window_type == "calendar"
                && policy.window_size.months == 0
                && policy.window_size.days == 1
                && policy.window_size.microseconds == 0)
            {
                continue;
            }
            latest
                .entry(policy.scope_id.clone())
                .and_modify(|(created_at, amount)| {
                    if policy.created_at > *created_at {
                        *created_at = policy.created_at;
                        *amount = policy.limit_amount;
                    }
                })
                .or_insert((policy.created_at, policy.limit_amount));
        }
        Ok(latest
            .into_iter()
            .map(|(scope_id, (_, amount))| (scope_id, amount))
            .collect())
    }

    async fn matching(
        &self,
        key_hash: &str,
        usage_pool_id: Option<&str>,
    ) -> Result<Vec<BudgetPolicy>, ServiceError> {
        Ok(self
            .policies
            .iter()
            .filter(|entry| {
                let policy = entry.value();
                policy.enabled
                    && ((policy.scope == "api_key" && policy.scope_id == key_hash)
                        || (policy.scope == "usage_pool"
                            && usage_pool_id.is_some_and(|pool| pool == policy.scope_id)))
            })
            .map(|entry| entry.value().clone())
            .collect())
    }

    async fn get(&self, id: &str) -> Result<Option<BudgetPolicy>, ServiceError> {
        Ok(self.policies.get(id).map(|p| p.value().clone()))
    }

    async fn list(&self) -> Result<Vec<BudgetPolicy>, ServiceError> {
        Ok(self.policies.iter().map(|p| p.value().clone()).collect())
    }
    async fn create(&self, policy: BudgetPolicy) -> Result<BudgetPolicy, ServiceError> {
        if self.policies.iter().any(|entry| {
            let existing = entry.value();
            existing.scope == policy.scope
                && existing.scope_id == policy.scope_id
                && existing.period == policy.period
                && existing.window_type == "calendar"
                && existing.limit_kind == "tokens"
        }) {
            return Err(ServiceError::invalid_field(
                "scope_id",
                "already has a token budget for this period; update that budget instead",
            ));
        }
        self.insert(policy.clone())?;
        Ok(policy)
    }
    async fn update(
        &self,
        id: &str,
        amount: Option<u64>,
        enabled: Option<bool>,
    ) -> Result<BudgetPolicy, ServiceError> {
        let mut policy = self
            .policies
            .get(id)
            .map(|p| p.value().clone())
            .ok_or_else(|| ServiceError::not_found(format!("Usage budget '{id}'")))?;
        if let Some(amount) = amount {
            policy.limit_amount = amount;
        }
        if let Some(enabled) = enabled {
            policy.enabled = enabled;
        }
        self.policies.insert(id.to_string(), policy.clone());
        Ok(policy)
    }
    async fn delete(&self, id: &str) -> Result<(), ServiceError> {
        self.policies.remove(id);
        Ok(())
    }
}

pub struct PostgresBudgetPolicyStore {
    pool: sqlx::PgPool,
    environment_id: String,
}

impl PostgresBudgetPolicyStore {
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
            return ServiceError::invalid_field(
                "scope_id",
                "already has a token budget for this period; update that budget instead",
            );
        }
        tracing::error!(%error, "budget policy store database error");
        ServiceError::internal("database error")
    }

    fn row_to_policy(row: &sqlx::postgres::PgRow) -> Result<BudgetPolicy, ServiceError> {
        use sqlx::Row;
        Ok(BudgetPolicy {
            quota_policy_id: row.try_get("quota_policy_id").map_err(Self::db_err)?,
            id: row.try_get("id").map_err(Self::db_err)?,
            environment_id: row.try_get("environment_id").map_err(Self::db_err)?,
            scope: row.try_get("scope").map_err(Self::db_err)?,
            scope_id: row.try_get("scope_id").map_err(Self::db_err)?,
            period: row.try_get("period").map_err(Self::db_err)?,
            window_type: row.try_get("window_type").map_err(Self::db_err)?,
            window_size: row.try_get("window_size").map_err(Self::db_err)?,
            anchor: row.try_get("anchor").map_err(Self::db_err)?,
            timezone: row.try_get("timezone").map_err(Self::db_err)?,
            limit_kind: row.try_get("limit_kind").map_err(Self::db_err)?,
            limit_amount: row
                .try_get::<i64, _>("limit_amount")
                .map_err(Self::db_err)?
                .max(0) as u64,
            enabled: row.try_get("enabled").map_err(Self::db_err)?,
            created_at: row.try_get("created_at").map_err(Self::db_err)?,
        })
    }
}

#[async_trait]
impl BudgetPolicyStore for PostgresBudgetPolicyStore {
    fn co_locates_key_writes(&self) -> bool {
        true
    }

    async fn set_key_daily_budget(
        &self,
        key_hash: &str,
        daily_token_budget: Option<u64>,
    ) -> Result<(), ServiceError> {
        let amount = daily_token_budget
            .map(|amount| {
                i64::try_from(amount).map_err(|_| {
                    ServiceError::invalid_field("daily_token_budget", "must fit PostgreSQL BIGINT")
                })
            })
            .transpose()?;
        let budget_id = uuid::Uuid::new_v4().to_string();
        let quota_policy_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "WITH removed AS ( \
                DELETE FROM chalk_router.budget_limits \
                WHERE environment_id = $1 AND scope = 'api_key' AND scope_id = $2 \
                  AND limit_kind = 'tokens' AND window_type = 'calendar' \
                  AND window_size = INTERVAL '1 day' \
             ), budget AS ( \
                INSERT INTO chalk_router.budget_limits \
                    (environment_id, id, scope, scope_id, window_type, window_size, \
                     limit_kind, limit_amount, enabled) \
                SELECT $1, $3, 'api_key', $2, 'calendar', INTERVAL '1 day', \
                       'tokens', $4, true \
                WHERE $4::bigint IS NOT NULL \
                RETURNING id, created_at \
             ) \
             INSERT INTO chalk_router.quota_policies \
                (id, environment_id, budget_limit_id, created_at) \
             SELECT $5, $1, id, created_at FROM budget",
        )
        .bind(&self.environment_id)
        .bind(key_hash)
        .bind(budget_id)
        .bind(amount)
        .bind(quota_policy_id)
        .execute(&self.pool)
        .await
        .map_err(Self::db_err)?;
        Ok(())
    }

    async fn replace_key_budgets(
        &self,
        key_hash: &str,
        budgets: Vec<ApiKeyBudgetInput>,
    ) -> Result<(), ServiceError> {
        let mut transaction = self.pool.begin().await.map_err(Self::db_err)?;
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
        .await
        .map_err(Self::db_err)?;
        sqlx::query(
            "DELETE FROM chalk_router.budget_limits \
             WHERE environment_id = $1 AND scope = 'api_key' AND scope_id = $2 \
               AND limit_kind = 'tokens' AND window_type = 'calendar' \
               AND timezone = 'UTC' AND period IN ('daily', 'weekly', 'monthly')",
        )
        .bind(&self.environment_id)
        .bind(key_hash)
        .execute(&mut *transaction)
        .await
        .map_err(Self::db_err)?;
        for budget in budgets {
            let id = uuid::Uuid::new_v4().to_string();
            let policy_id = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO chalk_router.budget_limits \
                 (id, environment_id, scope, scope_id, period, window_type, window_size, limit_kind, limit_amount, enabled) \
                 VALUES ($1, $2, 'api_key', $3, $4, 'calendar', \
                    CASE $4 WHEN 'weekly' THEN INTERVAL '7 days' WHEN 'monthly' THEN INTERVAL '1 month' ELSE INTERVAL '1 day' END, \
                    'tokens', $5, $6)",
            )
            .bind(&id)
            .bind(&self.environment_id)
            .bind(key_hash)
            .bind(&budget.period)
            .bind(i64::try_from(budget.token_limit).map_err(|_| ServiceError::invalid_field("token_limit", "must fit PostgreSQL BIGINT"))?)
            .bind(budget.enabled)
            .execute(&mut *transaction)
            .await
            .map_err(Self::db_err)?;
            sqlx::query("INSERT INTO chalk_router.quota_policies (id, environment_id, budget_limit_id) VALUES ($1, $2, $3)")
                .bind(policy_id)
                .bind(&self.environment_id)
                .bind(id)
                .execute(&mut *transaction)
                .await
                .map_err(Self::db_err)?;
        }
        transaction.commit().await.map_err(Self::db_err)
    }

    async fn get_key_daily_budget(&self, key_hash: &str) -> Result<Option<u64>, ServiceError> {
        let amount: Option<i64> = sqlx::query_scalar(
            "SELECT limit_amount FROM chalk_router.budget_limits \
             WHERE environment_id = $1 AND scope = 'api_key' AND scope_id = $2 \
               AND limit_kind = 'tokens' AND window_type = 'calendar' \
               AND window_size = INTERVAL '1 day' AND enabled \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&self.environment_id)
        .bind(key_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(Self::db_err)?;
        Ok(amount.map(|amount| amount.max(0) as u64))
    }

    async fn get_key_daily_budgets(
        &self,
        key_hashes: &[String],
    ) -> Result<HashMap<String, u64>, ServiceError> {
        if key_hashes.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT DISTINCT ON (scope_id) scope_id, limit_amount \
             FROM chalk_router.budget_limits \
             WHERE environment_id = $1 AND scope = 'api_key' \
               AND scope_id = ANY($2) AND limit_kind = 'tokens' \
               AND window_type = 'calendar' \
               AND window_size = INTERVAL '1 day' AND enabled \
             ORDER BY scope_id, created_at DESC",
        )
        .bind(&self.environment_id)
        .bind(key_hashes)
        .fetch_all(&self.pool)
        .await
        .map_err(Self::db_err)?;
        Ok(rows
            .into_iter()
            .map(|(scope_id, amount)| (scope_id, amount.max(0) as u64))
            .collect())
    }

    async fn matching(
        &self,
        key_hash: &str,
        usage_pool_id: Option<&str>,
    ) -> Result<Vec<BudgetPolicy>, ServiceError> {
        let rows = sqlx::query(
            "SELECT policy.id AS quota_policy_id, limits.* \
             FROM chalk_router.budget_limits AS limits \
             JOIN chalk_router.quota_policies AS policy \
               ON policy.environment_id = limits.environment_id \
              AND policy.budget_limit_id = limits.id \
             WHERE limits.environment_id = $1 AND limits.enabled \
               AND ((limits.scope = 'api_key' AND limits.scope_id = $2) \
                 OR (limits.scope = 'usage_pool' AND $3::text IS NOT NULL \
                     AND limits.scope_id = $3)) \
             ORDER BY policy.id",
        )
        .bind(&self.environment_id)
        .bind(key_hash)
        .bind(usage_pool_id)
        .fetch_all(&self.pool)
        .await
        .map_err(Self::db_err)?;
        rows.iter().map(Self::row_to_policy).collect()
    }

    async fn get(&self, id: &str) -> Result<Option<BudgetPolicy>, ServiceError> {
        let row = sqlx::query(
            "SELECT policy.id AS quota_policy_id, limits.* \
             FROM chalk_router.budget_limits AS limits \
             JOIN chalk_router.quota_policies AS policy \
               ON policy.environment_id = limits.environment_id \
              AND policy.budget_limit_id = limits.id \
             WHERE limits.environment_id = $1 AND limits.id = $2",
        )
        .bind(&self.environment_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(Self::db_err)?;
        row.as_ref().map(Self::row_to_policy).transpose()
    }

    async fn list(&self) -> Result<Vec<BudgetPolicy>, ServiceError> {
        let rows = sqlx::query("SELECT policy.id AS quota_policy_id, limits.* FROM chalk_router.budget_limits limits JOIN chalk_router.quota_policies policy ON policy.environment_id = limits.environment_id AND policy.budget_limit_id = limits.id WHERE limits.environment_id = $1 ORDER BY limits.created_at DESC").bind(&self.environment_id).fetch_all(&self.pool).await.map_err(Self::db_err)?;
        rows.iter().map(Self::row_to_policy).collect()
    }
    async fn create(&self, policy: BudgetPolicy) -> Result<BudgetPolicy, ServiceError> {
        let amount = i64::try_from(policy.limit_amount).map_err(|_| {
            ServiceError::invalid_field("daily_token_budget", "must fit PostgreSQL BIGINT")
        })?;
        let mut transaction = self.pool.begin().await.map_err(Self::db_err)?;
        sqlx::query("INSERT INTO chalk_router.budget_limits (id, environment_id, scope, scope_id, period, window_type, window_size, limit_kind, limit_amount, enabled) VALUES ($1,$2,$3,$4,$5,'calendar',CASE $5 WHEN 'weekly' THEN INTERVAL '7 days' WHEN 'monthly' THEN INTERVAL '1 month' ELSE INTERVAL '1 day' END,'tokens',$6,$7)")
            .bind(&policy.id).bind(&self.environment_id).bind(&policy.scope).bind(&policy.scope_id)
            .bind(&policy.period).bind(amount).bind(policy.enabled).execute(&mut *transaction).await.map_err(Self::db_err)?;
        sqlx::query("INSERT INTO chalk_router.quota_policies (id, environment_id, budget_limit_id) VALUES ($1,$2,$3)")
            .bind(&policy.quota_policy_id).bind(&self.environment_id).bind(&policy.id)
            .execute(&mut *transaction).await.map_err(Self::db_err)?;
        transaction.commit().await.map_err(Self::db_err)?;
        Ok(policy)
    }
    async fn update(
        &self,
        id: &str,
        amount: Option<u64>,
        enabled: Option<bool>,
    ) -> Result<BudgetPolicy, ServiceError> {
        let row = sqlx::query(
            "UPDATE chalk_router.budget_limits AS limits \
             SET limit_amount = COALESCE($3, limits.limit_amount), \
                 enabled = COALESCE($4, limits.enabled) \
             FROM chalk_router.quota_policies AS policy \
             WHERE limits.environment_id = $1 AND limits.id = $2 \
               AND policy.environment_id = limits.environment_id \
               AND policy.budget_limit_id = limits.id \
             RETURNING policy.id AS quota_policy_id, limits.*",
        )
        .bind(&self.environment_id)
        .bind(id)
        .bind(amount.map(|value| value as i64))
        .bind(enabled)
        .fetch_optional(&self.pool)
        .await
        .map_err(Self::db_err)?
        .ok_or_else(|| ServiceError::not_found(format!("Usage budget '{id}'")))?;
        Self::row_to_policy(&row)
    }
    async fn delete(&self, id: &str) -> Result<(), ServiceError> {
        let mut transaction = self.pool.begin().await.map_err(Self::db_err)?;
        // Delete the bridge explicitly so this remains correct even if a
        // deployment predates the FK cascade added by later migrations.
        sqlx::query("DELETE FROM chalk_router.quota_policies WHERE environment_id=$1 AND budget_limit_id=$2")
            .bind(&self.environment_id)
            .bind(id)
            .execute(&mut *transaction)
            .await
            .map_err(Self::db_err)?;
        let deleted =
            sqlx::query("DELETE FROM chalk_router.budget_limits WHERE environment_id=$1 AND id=$2")
                .bind(&self.environment_id)
                .bind(id)
                .execute(&mut *transaction)
                .await
                .map_err(Self::db_err)?;
        if deleted.rows_affected() == 0 {
            return Err(ServiceError::not_found(format!("Usage budget '{id}'")));
        }
        transaction.commit().await.map_err(Self::db_err)?;
        Ok(())
    }
}

pub fn from_core_store(
    pool: Option<sqlx::PgPool>,
    environment_id: String,
) -> Arc<dyn BudgetPolicyStore> {
    let inner: Arc<dyn BudgetPolicyStore> = match pool {
        Some(pool) => Arc::new(PostgresBudgetPolicyStore::new(pool, environment_id)),
        None => Arc::new(MemoryBudgetPolicyStore::new(environment_id)),
    };
    Arc::new(crate::quota_definition_cache::CachedBudgetPolicyStore::new(
        inner,
    ))
}
