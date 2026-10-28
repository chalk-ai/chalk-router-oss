//! Usage pools: named groupings of API keys that share budgets / rate limits.
//! (Formerly "teams"; renamed to decouple from platform identity — a pool is a
//! router-local budgeting bucket, not a chalk-platform Team.)

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::budget::ApiKeyBudgetInput;
use crate::ratelimit::{scoped_rate_limit_policies, ApiKeyRateLimitInput};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsagePool {
    pub id: String,
    pub name: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Persistence failure from a [`UsagePoolStore`] write.
#[derive(Debug)]
pub enum UsagePoolStoreError {
    DuplicateName,
    InvalidRateLimitConnection,
    Internal(String),
}

impl std::fmt::Display for UsagePoolStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UsagePoolStoreError::DuplicateName => {
                write!(f, "a usage pool with this name already exists")
            }
            UsagePoolStoreError::InvalidRateLimitConnection => {
                write!(f, "rate limit connection_id does not exist")
            }
            UsagePoolStoreError::Internal(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for UsagePoolStoreError {}

impl From<sqlx::Error> for UsagePoolStoreError {
    fn from(e: sqlx::Error) -> Self {
        if matches!(
            crate::db_error::classify(&e),
            Some(crate::db_error::DbConstraint::Unique)
        ) && e.as_database_error().and_then(|db| db.constraint())
            == Some("uq_router_usage_pools_name")
        {
            return UsagePoolStoreError::DuplicateName;
        }
        if matches!(
            crate::db_error::classify(&e),
            Some(crate::db_error::DbConstraint::ForeignKey)
        ) && e.as_database_error().and_then(|db| db.constraint())
            == Some("rate_limits_connection_fk")
        {
            return UsagePoolStoreError::InvalidRateLimitConnection;
        }
        UsagePoolStoreError::Internal(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait UsagePoolStore: Send + Sync {
    async fn create_pool(&self, name: String) -> Result<UsagePool, UsagePoolStoreError>;
    async fn update_pool(
        &self,
        id: &str,
        name: String,
    ) -> Result<Option<UsagePool>, UsagePoolStoreError>;
    /// Atomically create a pool and its co-located quota definitions.
    /// `Ok(None)` means this store cannot provide a shared transaction, so the
    /// service must create the definitions separately and compensate on error.
    async fn create_with_quota_definitions(
        &self,
        _name: String,
        _rate_limits: Vec<ApiKeyRateLimitInput>,
        _usage_budgets: Vec<ApiKeyBudgetInput>,
    ) -> Result<Option<UsagePool>, UsagePoolStoreError> {
        Ok(None)
    }
    /// Atomically update a pool and any selected co-located quota sets. The
    /// outer `Option` means unsupported; the inner option means pool not found.
    async fn update_with_quota_definitions(
        &self,
        _id: &str,
        _name: Option<String>,
        _rate_limits: Option<Vec<ApiKeyRateLimitInput>>,
        _usage_budgets: Option<Vec<ApiKeyBudgetInput>>,
    ) -> Result<Option<Option<UsagePool>>, UsagePoolStoreError> {
        Ok(None)
    }
    async fn list_pools(&self) -> Vec<UsagePool>;
    /// Point lookup by id. Callers that already know the id should prefer
    /// this over scanning `list_pools`/`list_pools_page`.
    async fn get_pool(&self, id: &str) -> Result<Option<UsagePool>, UsagePoolStoreError>;
    /// Exact, case-sensitive lookup in this store's environment.
    async fn get_pool_by_name(&self, name: &str) -> Result<Option<UsagePool>, UsagePoolStoreError>;
    /// Cursor-paginated variant of [`UsagePoolStore::list_pools`]: returns up
    /// to `fetch` pools with `created_at < before`, ordered newest first
    /// (`created_at DESC`). Callers fetch `limit + 1` rows to detect whether
    /// another page exists (see `service::pagination`). Unlike `list_pools`,
    /// a database error propagates: a partial pagination walk that silently
    /// reads a failed page as "no more rows" would be indistinguishable from
    /// a complete result set.
    async fn list_pools_page(
        &self,
        before: chrono::DateTime<chrono::Utc>,
        fetch: i64,
    ) -> Result<Vec<UsagePool>, UsagePoolStoreError>;
    async fn delete_pool(&self, id: &str) -> Result<bool, UsagePoolStoreError>;
}

// ---------------------------------------------------------------------------
// PostgreSQL implementation
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct PostgresUsagePoolStore {
    pool: sqlx::PgPool,
    /// Tenancy scope (see `PostgresApiKeyStore`).
    environment_id: String,
}

impl PostgresUsagePoolStore {
    /// Wrap an existing pool. Tables are provisioned by migrations before boot.
    pub fn new(pool: sqlx::PgPool, environment_id: String) -> Self {
        Self {
            pool,
            environment_id,
        }
    }

    async fn insert_usage_budgets(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        pool_id: &str,
        usage_budgets: Vec<ApiKeyBudgetInput>,
    ) -> Result<(), UsagePoolStoreError> {
        for budget in usage_budgets {
            let budget_id = uuid::Uuid::new_v4().to_string();
            let quota_policy_id = uuid::Uuid::new_v4().to_string();
            let limit_amount = i64::try_from(budget.token_limit).map_err(|_| {
                UsagePoolStoreError::Internal(
                    "usage budget token_limit must fit BIGINT".to_string(),
                )
            })?;
            sqlx::query(
                "WITH budget AS ( \
                    INSERT INTO chalk_router.budget_limits \
                        (id, environment_id, scope, scope_id, period, window_type, window_size, limit_kind, limit_amount, enabled) \
                    VALUES ($1, $2, 'usage_pool', $3, $4, 'calendar', \
                        CASE $4 WHEN 'weekly' THEN INTERVAL '7 days' WHEN 'monthly' THEN INTERVAL '1 month' ELSE INTERVAL '1 day' END, \
                        'tokens', $5, $6) \
                    RETURNING id, created_at \
                 ) \
                 INSERT INTO chalk_router.quota_policies \
                    (id, environment_id, budget_limit_id, created_at) \
                 SELECT $7, $2, id, created_at FROM budget",
            )
            .bind(&budget_id)
            .bind(&self.environment_id)
            .bind(pool_id)
            .bind(&budget.period)
            .bind(limit_amount)
            .bind(budget.enabled)
            .bind(&quota_policy_id)
            .execute(&mut **transaction)
            .await?;
        }
        Ok(())
    }

    async fn insert_rate_limits(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        pool_id: &str,
        rate_limits: Vec<ApiKeyRateLimitInput>,
    ) -> Result<(), UsagePoolStoreError> {
        for policy in scoped_rate_limit_policies("usage_pool", pool_id, &rate_limits) {
            let limit_value = i64::try_from(policy.limit_value).map_err(|_| {
                UsagePoolStoreError::Internal("rate limit must fit BIGINT".to_string())
            })?;
            let burst_value = policy
                .burst_value
                .map(|value| {
                    i64::try_from(value).map_err(|_| {
                        UsagePoolStoreError::Internal(
                            "rate limit burst must fit BIGINT".to_string(),
                        )
                    })
                })
                .transpose()?;
            sqlx::query(
                "WITH limits AS ( \
                    INSERT INTO chalk_router.rate_limits \
                        (environment_id, id, limit_type, limit_value, connection_id, models, burst_value, scope, scope_id, enabled, created_at) \
                    VALUES ($1, $2, $3, $4, $5, $6, $7, 'usage_pool', $8, $9, $10) \
                    RETURNING id \
                 ) \
                 INSERT INTO chalk_router.quota_policies \
                    (id, environment_id, rate_limit_id, created_at) \
                 SELECT $11, $1, id, $10 FROM limits",
            )
            .bind(&self.environment_id)
            .bind(&policy.id)
            .bind(&policy.limit_type)
            .bind(limit_value)
            .bind(&policy.connection_id)
            .bind(&policy.models)
            .bind(burst_value)
            .bind(pool_id)
            .bind(policy.enabled)
            .bind(policy.created_at)
            .bind(&policy.quota_policy_id)
            .execute(&mut **transaction)
            .await?;
        }
        Ok(())
    }
}

#[async_trait]
impl UsagePoolStore for PostgresUsagePoolStore {
    async fn create_pool(&self, name: String) -> Result<UsagePool, UsagePoolStoreError> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now();
        sqlx::query(
            "INSERT INTO chalk_router.usage_pools (environment_id, id, name, created_at) VALUES ($1, $2, $3, $4)",
        )
        .bind(&self.environment_id)
        .bind(&id)
        .bind(&name)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(UsagePool {
            id,
            name,
            created_at: now,
        })
    }

    async fn update_pool(
        &self,
        id: &str,
        name: String,
    ) -> Result<Option<UsagePool>, UsagePoolStoreError> {
        Ok(
            sqlx::query_as::<_, (String, String, chrono::DateTime<chrono::Utc>)>(
                "UPDATE chalk_router.usage_pools SET name = $3 \
             WHERE environment_id = $1 AND id = $2 \
             RETURNING id, name, created_at",
            )
            .bind(&self.environment_id)
            .bind(id)
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
            .map(|(id, name, created_at)| UsagePool {
                id,
                name,
                created_at,
            }),
        )
    }

    async fn create_with_quota_definitions(
        &self,
        name: String,
        rate_limits: Vec<ApiKeyRateLimitInput>,
        usage_budgets: Vec<ApiKeyBudgetInput>,
    ) -> Result<Option<UsagePool>, UsagePoolStoreError> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now();
        let mut transaction = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO chalk_router.usage_pools (environment_id, id, name, created_at) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(&self.environment_id)
        .bind(&id)
        .bind(&name)
        .bind(now)
        .execute(&mut *transaction)
        .await?;

        self.insert_usage_budgets(&mut transaction, &id, usage_budgets)
            .await?;
        self.insert_rate_limits(&mut transaction, &id, rate_limits)
            .await?;

        transaction.commit().await?;
        Ok(Some(UsagePool {
            id,
            name,
            created_at: now,
        }))
    }

    async fn update_with_quota_definitions(
        &self,
        id: &str,
        name: Option<String>,
        rate_limits: Option<Vec<ApiKeyRateLimitInput>>,
        usage_budgets: Option<Vec<ApiKeyBudgetInput>>,
    ) -> Result<Option<Option<UsagePool>>, UsagePoolStoreError> {
        let mut transaction = self.pool.begin().await?;
        let Some((pool_id, mut pool_name, created_at)) =
            sqlx::query_as::<_, (String, String, chrono::DateTime<chrono::Utc>)>(
                "SELECT id, name, created_at FROM chalk_router.usage_pools \
             WHERE environment_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(&self.environment_id)
            .bind(id)
            .fetch_optional(&mut *transaction)
            .await?
        else {
            transaction.commit().await?;
            return Ok(Some(None));
        };

        if let Some(name) = name {
            sqlx::query(
                "UPDATE chalk_router.usage_pools SET name = $3 \
                 WHERE environment_id = $1 AND id = $2",
            )
            .bind(&self.environment_id)
            .bind(id)
            .bind(&name)
            .execute(&mut *transaction)
            .await?;
            pool_name = name;
        }
        if let Some(usage_budgets) = usage_budgets {
            sqlx::query(
                "DELETE FROM chalk_router.budget_limits \
                 WHERE environment_id = $1 AND scope = 'usage_pool' AND scope_id = $2 \
                   AND limit_kind = 'tokens' AND window_type = 'calendar' \
                   AND timezone = 'UTC' AND period IN ('daily', 'weekly', 'monthly')",
            )
            .bind(&self.environment_id)
            .bind(id)
            .execute(&mut *transaction)
            .await?;
            self.insert_usage_budgets(&mut transaction, id, usage_budgets)
                .await?;
        }
        if let Some(rate_limits) = rate_limits {
            sqlx::query(
                "DELETE FROM chalk_router.rate_limits \
                 WHERE environment_id = $1 AND scope = 'usage_pool' AND scope_id = $2",
            )
            .bind(&self.environment_id)
            .bind(id)
            .execute(&mut *transaction)
            .await?;
            self.insert_rate_limits(&mut transaction, id, rate_limits)
                .await?;
        }

        transaction.commit().await?;
        Ok(Some(Some(UsagePool {
            id: pool_id,
            name: pool_name,
            created_at,
        })))
    }

    async fn list_pools(&self) -> Vec<UsagePool> {
        sqlx::query_as::<_, (String, String, chrono::DateTime<chrono::Utc>)>(
            "SELECT id, name, created_at FROM chalk_router.usage_pools WHERE environment_id = $1 ORDER BY created_at ASC",
        )
        .bind(&self.environment_id)
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(id, name, created_at)| UsagePool { id, name, created_at })
        .collect()
    }

    async fn list_pools_page(
        &self,
        before: chrono::DateTime<chrono::Utc>,
        fetch: i64,
    ) -> Result<Vec<UsagePool>, UsagePoolStoreError> {
        Ok(sqlx::query_as::<_, (String, String, chrono::DateTime<chrono::Utc>)>(
            "SELECT id, name, created_at FROM chalk_router.usage_pools WHERE environment_id = $1 AND created_at < $2 \
             ORDER BY created_at DESC LIMIT $3",
        )
        .bind(&self.environment_id)
        .bind(before)
        .bind(fetch)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|(id, name, created_at)| UsagePool { id, name, created_at })
        .collect())
    }

    async fn get_pool(&self, id: &str) -> Result<Option<UsagePool>, UsagePoolStoreError> {
        Ok(sqlx::query_as::<_, (String, String, chrono::DateTime<chrono::Utc>)>(
            "SELECT id, name, created_at FROM chalk_router.usage_pools WHERE environment_id = $1 AND id = $2",
        )
        .bind(&self.environment_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .map(|(id, name, created_at)| UsagePool { id, name, created_at }))
    }

    async fn get_pool_by_name(&self, name: &str) -> Result<Option<UsagePool>, UsagePoolStoreError> {
        Ok(sqlx::query_as::<_, (String, String, chrono::DateTime<chrono::Utc>)>(
            "SELECT id, name, created_at FROM chalk_router.usage_pools WHERE environment_id = $1 AND name = $2",
        )
        .bind(&self.environment_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?
        .map(|(id, name, created_at)| UsagePool { id, name, created_at }))
    }

    async fn delete_pool(&self, id: &str) -> Result<bool, UsagePoolStoreError> {
        // Assigned keys, owned quota definitions, and the pool are removed in
        // one statement so callers never observe a partially deleted pool.
        let deleted = sqlx::query_scalar::<_, String>(
            r#"
            WITH target AS (
                SELECT id
                FROM chalk_router.usage_pools
                WHERE environment_id = $1 AND id = $2
                FOR UPDATE
            ), detached AS (
                UPDATE chalk_router.api_keys
                SET usage_pool_id = NULL
                WHERE environment_id = $1
                  AND usage_pool_id = (SELECT id FROM target)
                RETURNING 1
            ), removed_budgets AS (
                DELETE FROM chalk_router.budget_limits
                WHERE environment_id = $1
                  AND scope = 'usage_pool'
                  AND scope_id = (SELECT id FROM target)
                RETURNING 1
            ), removed_rate_limits AS (
                DELETE FROM chalk_router.rate_limits
                WHERE environment_id = $1
                  AND scope = 'usage_pool'
                  AND scope_id = (SELECT id FROM target)
                RETURNING 1
            )
            DELETE FROM chalk_router.usage_pools
            WHERE environment_id = $1
              AND id = $2
              AND EXISTS (SELECT 1 FROM target)
              -- Forces detached to finish before the immediate FK check.
              AND (SELECT count(*) FROM detached) >= 0
              AND (SELECT count(*) FROM removed_budgets) >= 0
              AND (SELECT count(*) FROM removed_rate_limits) >= 0
            RETURNING id
            "#,
        )
        .bind(&self.environment_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(deleted.is_some())
    }
}

// ---------------------------------------------------------------------------
// In-memory implementation
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct InMemoryUsagePoolStore {
    pools: Arc<Mutex<HashMap<String, UsagePool>>>,
}

impl InMemoryUsagePoolStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl UsagePoolStore for InMemoryUsagePoolStore {
    async fn create_pool(&self, name: String) -> Result<UsagePool, UsagePoolStoreError> {
        let mut pools = self.pools.lock().expect("usage pool store poisoned");
        if pools.values().any(|pool| pool.name == name) {
            return Err(UsagePoolStoreError::DuplicateName);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let pool = UsagePool {
            id: id.clone(),
            name,
            created_at: chrono::Utc::now(),
        };
        pools.insert(id, pool.clone());
        Ok(pool)
    }

    async fn update_pool(
        &self,
        id: &str,
        name: String,
    ) -> Result<Option<UsagePool>, UsagePoolStoreError> {
        let mut pools = self.pools.lock().expect("usage pool store poisoned");
        if !pools.contains_key(id) {
            return Ok(None);
        }
        if pools
            .values()
            .any(|pool| pool.id != id && pool.name == name)
        {
            return Err(UsagePoolStoreError::DuplicateName);
        }
        let pool = pools.get_mut(id).expect("pool exists under the same lock");
        pool.name = name;
        Ok(Some(pool.clone()))
    }

    async fn list_pools(&self) -> Vec<UsagePool> {
        let mut pools: Vec<_> = self
            .pools
            .lock()
            .expect("usage pool store poisoned")
            .values()
            .cloned()
            .collect();
        pools.sort_by_key(|p| p.created_at);
        pools
    }

    async fn list_pools_page(
        &self,
        before: chrono::DateTime<chrono::Utc>,
        fetch: i64,
    ) -> Result<Vec<UsagePool>, UsagePoolStoreError> {
        let mut pools: Vec<_> = self
            .pools
            .lock()
            .expect("usage pool store poisoned")
            .values()
            .filter(|p| p.created_at < before)
            .cloned()
            .collect();
        pools.sort_by_key(|pool| std::cmp::Reverse(pool.created_at));
        Ok(pools.into_iter().take(fetch.max(0) as usize).collect())
    }

    async fn get_pool(&self, id: &str) -> Result<Option<UsagePool>, UsagePoolStoreError> {
        Ok(self
            .pools
            .lock()
            .expect("usage pool store poisoned")
            .get(id)
            .cloned())
    }

    async fn get_pool_by_name(&self, name: &str) -> Result<Option<UsagePool>, UsagePoolStoreError> {
        Ok(self
            .pools
            .lock()
            .expect("usage pool store poisoned")
            .values()
            .find(|pool| pool.name == name)
            .cloned())
    }

    async fn delete_pool(&self, id: &str) -> Result<bool, UsagePoolStoreError> {
        Ok(self
            .pools
            .lock()
            .expect("usage pool store poisoned")
            .remove(id)
            .is_some())
    }
}
