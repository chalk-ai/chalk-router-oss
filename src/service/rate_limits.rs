use std::sync::Arc;

use crate::ratelimit::{
    normalize_rate_limit_identity_fields, validate_rate_limit_models, RateLimitPolicy,
    RateLimitPolicyStore,
};
use crate::service::pagination::{format_cursor, parse_cursor, resolve_limit};
use crate::service::ServiceError;

#[derive(Debug, Clone)]
pub struct CreateRateLimitInput {
    pub limit_type: String,
    pub limit_value: u64,
    pub burst_value: Option<u64>,
    pub connection_id: Option<String>,
    pub models: Vec<String>,
    pub scope: String,
    pub scope_id: String,
    pub enabled: bool,
}

#[derive(Debug, Default, Clone)]
pub struct UpdateRateLimitInput {
    pub enabled: Option<bool>,
    pub limit_type: Option<String>,
    pub limit_value: Option<u64>,
    pub burst_value: Option<Option<u64>>,
    pub connection_id: Option<Option<String>>,
    pub models: Option<Vec<String>>,
}

fn validate_and_normalize(policy: &mut RateLimitPolicy) -> Result<(), ServiceError> {
    policy.limit_type = policy.limit_type.trim().to_string();
    if !matches!(
        policy.limit_type.as_str(),
        "requests_per_minute" | "tokens_per_minute" | "concurrent_requests"
    ) {
        return Err(ServiceError::invalid_field(
            "limit_type",
            "must be requests_per_minute, tokens_per_minute, or concurrent_requests",
        ));
    }
    if policy.limit_value == 0 {
        return Err(ServiceError::invalid_field(
            "limit_value",
            "must be positive",
        ));
    }
    if policy.limit_value > i64::MAX as u64 {
        return Err(ServiceError::invalid_field(
            "limit_value",
            "must fit PostgreSQL BIGINT",
        ));
    }
    policy.burst_value = if policy.limit_type == "concurrent_requests" {
        if policy
            .burst_value
            .is_some_and(|burst| burst != policy.limit_value)
        {
            return Err(ServiceError::invalid_field(
                "burst_value",
                "must be absent or equal limit_value for concurrent_requests",
            ));
        }
        None
    } else {
        Some(policy.burst_value.unwrap_or(policy.limit_value))
    };
    if policy.burst_value == Some(0) {
        return Err(ServiceError::invalid_field(
            "burst_value",
            "must be positive",
        ));
    }
    if policy
        .burst_value
        .is_some_and(|burst| burst > i64::MAX as u64)
    {
        return Err(ServiceError::invalid_field(
            "burst_value",
            "must fit PostgreSQL BIGINT",
        ));
    }
    policy.connection_id = policy
        .connection_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    policy.models = policy
        .models
        .drain(..)
        .map(|model| model.trim().to_string())
        .filter(|model| !model.is_empty())
        .collect();
    policy.models.sort();
    policy.models.dedup();
    if policy.connection_id.is_none() && !policy.models.is_empty() {
        return Err(ServiceError::invalid_field(
            "models",
            "requires connection_id",
        ));
    }
    validate_rate_limit_models(&policy.models, "models")?;
    policy.scope = policy.scope.trim().to_string();
    if !matches!(policy.scope.as_str(), "api_key" | "usage_pool") {
        return Err(ServiceError::invalid_field(
            "scope",
            "must be api_key or usage_pool",
        ));
    }
    if policy.scope_id.trim().is_empty() {
        return Err(ServiceError::invalid_field(
            "scope_id",
            "must not be empty for api_key or usage_pool scope",
        ));
    }
    policy.scope_id = policy.scope_id.trim().to_string();
    normalize_rate_limit_identity_fields(policy);
    Ok(())
}

pub struct RateLimitService {
    store: Arc<dyn RateLimitPolicyStore>,
    quota_enabled: bool,
}

impl RateLimitService {
    pub fn new(store: Arc<dyn RateLimitPolicyStore>, quota_enabled: bool) -> Self {
        Self {
            store,
            quota_enabled,
        }
    }

    pub async fn create(
        &self,
        input: CreateRateLimitInput,
    ) -> Result<RateLimitPolicy, ServiceError> {
        let mut policy = RateLimitPolicy {
            quota_policy_id: uuid::Uuid::new_v4().to_string(),
            id: uuid::Uuid::new_v4().to_string(),
            limit_type: input.limit_type,
            limit_value: input.limit_value,
            burst_value: input.burst_value,
            connection_id: input.connection_id,
            models: input.models,
            scope: input.scope,
            scope_id: input.scope_id,
            enabled: input.enabled,
            created_at: chrono::Utc::now(),
        };
        // Match `update`'s gate: a disabled policy takes no effect, so it may
        // be authored even while quota enforcement is off; only an enabled
        // one is refused.
        if policy.enabled && !self.quota_enabled {
            return Err(ServiceError::failed_precondition(
                "quota enforcement is disabled (set ROUTER_QUOTA_BACKEND)",
            ));
        }
        validate_and_normalize(&mut policy)?;
        self.store.create(policy).await
    }

    pub async fn list(&self) -> Vec<RateLimitPolicy> {
        self.store.list().await
    }

    /// Cursor-paginated variant of [`RateLimitService::list`]. `limit`
    /// defaults to 40 (rejecting <= 0, clamping above 1000); `cursor`
    /// defaults to now(). Returns the page plus a `next_cursor` that is set
    /// iff more rows remain.
    pub async fn list_page(
        &self,
        limit: Option<i32>,
        cursor: Option<String>,
    ) -> Result<(Vec<RateLimitPolicy>, Option<String>), ServiceError> {
        let limit = resolve_limit(limit)?;
        let before = parse_cursor(cursor.as_deref())?;

        let mut page = self.store.list_page(before, limit + 1).await?;
        if page.len() as i64 > limit {
            page.truncate(limit as usize);
            let next_cursor = page.last().map(|p| format_cursor(p.created_at));
            Ok((page, next_cursor))
        } else {
            Ok((page, None))
        }
    }

    /// Point lookup by id.
    pub async fn get(&self, id: &str) -> Result<RateLimitPolicy, ServiceError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(ServiceError::invalid_field("id", "must not be empty"));
        }
        self.store.get(id).await
    }

    pub async fn update(
        &self,
        id: &str,
        input: UpdateRateLimitInput,
    ) -> Result<RateLimitPolicy, ServiceError> {
        let mut policy = self.store.get(id).await?;
        if let Some(enabled) = input.enabled {
            policy.enabled = enabled;
        }
        if let Some(limit_type) = input.limit_type {
            policy.limit_type = limit_type;
        }
        if let Some(limit_value) = input.limit_value {
            policy.limit_value = limit_value;
        }
        if let Some(burst_value) = input.burst_value {
            policy.burst_value = burst_value;
        }
        if let Some(connection_id) = input.connection_id {
            policy.connection_id = connection_id;
        }
        if let Some(models) = input.models {
            policy.models = models;
        }
        if policy.enabled && !self.quota_enabled {
            return Err(ServiceError::failed_precondition(
                "quota enforcement is disabled (set ROUTER_QUOTA_BACKEND)",
            ));
        }
        validate_and_normalize(&mut policy)?;
        self.store.update(policy).await
    }

    pub async fn delete(&self, id: &str) -> Result<(), ServiceError> {
        self.store.delete(id).await
    }
}
