//! Resolve public budget/rate-limit definitions into backend-neutral actions.

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use std::collections::HashSet;
use std::sync::Arc;

use crate::budget::{BudgetPolicy, BudgetPolicyStore};
use crate::quota::{QuotaAction, QuotaAlgorithm, QuotaMeter, QuotaSource, TokenReservation};
use crate::ratelimit::{RateLimitMatch, RateLimitModel, RateLimitPolicyStore};
use crate::service::ServiceError;

pub struct QuotaPolicyResolver {
    environment_id: String,
    budget_store: Arc<dyn BudgetPolicyStore>,
    rate_limit_store: Arc<dyn RateLimitPolicyStore>,
}

/// Backend-neutral actions plus an admission requirement that cannot be
/// represented as a zero-sized token action.
pub struct QuotaActionPlan {
    pub actions: Vec<QuotaAction>,
    pub requires_token_reservation: bool,
    pub requires_public_model: bool,
}

impl QuotaPolicyResolver {
    pub fn in_memory(environment_id: String) -> Arc<Self> {
        Arc::new(Self::new(
            environment_id.clone(),
            Arc::new(crate::budget::MemoryBudgetPolicyStore::new(
                environment_id.clone(),
            )),
            Arc::new(crate::ratelimit::MemoryRateLimitPolicyStore::new(
                environment_id,
            )),
        ))
    }

    pub fn new(
        environment_id: String,
        budget_store: Arc<dyn BudgetPolicyStore>,
        rate_limit_store: Arc<dyn RateLimitPolicyStore>,
    ) -> Self {
        Self {
            environment_id,
            budget_store,
            rate_limit_store,
        }
    }

    pub async fn budget_actions(
        &self,
        key_hash: &str,
        usage_pool_id: Option<&str>,
        reservation: TokenReservation,
        now: DateTime<Utc>,
    ) -> Result<Vec<QuotaAction>, ServiceError> {
        let policies = self
            .budget_store
            .matching_shared(key_hash, usage_pool_id)
            .await?;
        let mut actions = Vec::with_capacity(policies.len());
        for policy in policies.iter() {
            actions.push(self.compile_budget(policy, reservation, now)?);
        }
        validate_compiled(&actions)?;
        Ok(actions)
    }

    /// Whether any enabled request-level budget or rate-limit definition
    /// applies to this caller. This is a read-only policy-presence check: it
    /// deliberately does not acquire or mutate quota state.
    pub async fn has_request_policies(
        &self,
        key_hash: &str,
        usage_pool_id: Option<&str>,
    ) -> Result<bool, ServiceError> {
        if !self
            .budget_store
            .matching_shared(key_hash, usage_pool_id)
            .await?
            .is_empty()
        {
            return Ok(true);
        }
        let request = RateLimitMatch {
            key_hash,
            usage_pool_id,
            connection_id: None,
            model: RateLimitModel::NotApplicable,
        };
        Ok(!self
            .rate_limit_store
            .matching_shared(&request)
            .await?
            .is_empty())
    }

    /// Whether any enabled route-level rate-limit definition applies to one
    /// resolved dispatch. Like [`Self::has_request_policies`], this does not
    /// touch mutable quota state.
    pub async fn has_rate_limit_policies(
        &self,
        request: &RateLimitMatch<'_>,
    ) -> Result<bool, ServiceError> {
        Ok(!self
            .rate_limit_store
            .matching_shared(request)
            .await?
            .is_empty())
    }

    /// Compile every request-level policy. Budgets and rate limits without a
    /// connection selector are acquired together once for the logical request.
    pub async fn request_actions(
        &self,
        key_hash: &str,
        usage_pool_id: Option<&str>,
        token_reservation: TokenReservation,
        now: DateTime<Utc>,
    ) -> Result<QuotaActionPlan, ServiceError> {
        let (mut actions, mut requires_token_reservation) = match token_reservation {
            TokenReservation::Known(_) | TokenReservation::SoftCap(_) => (
                self.budget_actions(key_hash, usage_pool_id, token_reservation, now)
                    .await?,
                false,
            ),
            TokenReservation::Unknown => {
                let policies = self
                    .budget_store
                    .matching_shared(key_hash, usage_pool_id)
                    .await?;
                for policy in policies.iter() {
                    self.validate_budget(policy, now)?;
                }
                (Vec::new(), !policies.is_empty())
            }
            TokenReservation::NotApplicable => (Vec::new(), false),
        };
        let request = RateLimitMatch {
            key_hash,
            usage_pool_id,
            connection_id: None,
            model: RateLimitModel::NotApplicable,
        };
        let mut rate_plan = self
            .compile_rate_actions(&request, token_reservation)
            .await?;
        actions.append(&mut rate_plan.actions);
        requires_token_reservation |= rate_plan.requires_token_reservation;
        validate_compiled(&actions)?;
        Ok(QuotaActionPlan {
            actions,
            requires_token_reservation,
            requires_public_model: false,
        })
    }

    pub async fn rate_actions(
        &self,
        request: &RateLimitMatch<'_>,
        token_reservation: TokenReservation,
    ) -> Result<QuotaActionPlan, ServiceError> {
        self.compile_rate_actions(request, token_reservation).await
    }

    async fn compile_rate_actions(
        &self,
        request: &RateLimitMatch<'_>,
        token_reservation: TokenReservation,
    ) -> Result<QuotaActionPlan, ServiceError> {
        let policies = self.rate_limit_store.matching_shared(request).await?;
        let mut actions = Vec::with_capacity(policies.len());
        let mut requires_token_reservation = false;
        let mut requires_public_model = false;
        for policy in policies.iter() {
            if policy.quota_policy_id.is_empty() {
                return Err(ServiceError::failed_precondition(format!(
                    "rate limit '{}' has no quota bridge",
                    policy.id
                )));
            }
            // Token-rate policies do not apply to operations without token
            // accounting, so they also must not force an otherwise optional
            // public model (for example, provider-default image generation).
            if policy.limit_type == "tokens_per_minute"
                && token_reservation == TokenReservation::NotApplicable
            {
                continue;
            }
            if matches!(request.model, RateLimitModel::ProviderSelected)
                && !policy.models.is_empty()
            {
                requires_public_model = true;
                continue;
            }
            let (algorithm, meter, amount, burst) = match policy.limit_type.as_str() {
                "requests_per_minute" => (
                    QuotaAlgorithm::TokenBucket,
                    QuotaMeter::Requests,
                    1,
                    Some(policy.burst_value.unwrap_or(policy.limit_value)),
                ),
                "tokens_per_minute" if token_reservation == TokenReservation::Unknown => {
                    requires_token_reservation = true;
                    continue;
                }
                "tokens_per_minute" => {
                    let burst = policy.burst_value.unwrap_or(policy.limit_value);
                    (
                        QuotaAlgorithm::TokenBucket,
                        QuotaMeter::Tokens,
                        token_reservation
                            .amount_for(burst)
                            .expect("unknown and inapplicable reservations handled above"),
                        Some(burst),
                    )
                }
                "concurrent_requests" => (
                    QuotaAlgorithm::Concurrency,
                    QuotaMeter::ConcurrentRequests,
                    1,
                    None,
                ),
                other => {
                    return Err(ServiceError::failed_precondition(format!(
                        "unsupported rate-limit type '{other}'"
                    )))
                }
            };
            actions.push(QuotaAction {
                policy_id: policy.quota_policy_id.clone(),
                environment_id: self.environment_id.clone(),
                source: QuotaSource::RateLimit,
                algorithm,
                meter,
                limit_value: policy.limit_value,
                burst,
                amount,
                window_start: None,
                window_end: None,
            });
        }
        validate_compiled(&actions)?;
        Ok(QuotaActionPlan {
            actions,
            requires_token_reservation,
            requires_public_model,
        })
    }

    fn validate_budget(
        &self,
        policy: &BudgetPolicy,
        now: DateTime<Utc>,
    ) -> Result<(DateTime<Utc>, DateTime<Utc>), ServiceError> {
        if policy.environment_id != self.environment_id {
            return Err(ServiceError::failed_precondition(
                "budget bridge crossed an environment boundary",
            ));
        }
        if policy.quota_policy_id.is_empty() {
            return Err(ServiceError::failed_precondition(format!(
                "budget '{}' has no quota bridge",
                policy.id
            )));
        }
        if policy.window_type != "calendar" {
            return Err(ServiceError::invalid_field(
                "window_type",
                "rolling budgets are not supported by quota enforcement",
            ));
        }
        if policy.limit_kind != "tokens" {
            return Err(ServiceError::invalid_field(
                "limit_kind",
                "cost budgets are not supported by quota enforcement",
            ));
        }
        if !matches!(policy.scope.as_str(), "api_key" | "usage_pool") {
            return Err(ServiceError::invalid_field(
                "scope",
                "budget scope must be api_key or usage_pool",
            ));
        }
        if policy.limit_amount == 0 {
            return Err(ServiceError::invalid_field(
                "limit_amount",
                "must be positive",
            ));
        }
        period_window(now, &policy.period)
    }

    fn compile_budget(
        &self,
        policy: &BudgetPolicy,
        reservation: TokenReservation,
        now: DateTime<Utc>,
    ) -> Result<QuotaAction, ServiceError> {
        let (window_start, window_end) = self.validate_budget(policy, now)?;
        let amount = reservation.amount_for(policy.limit_amount).ok_or_else(|| {
            ServiceError::failed_precondition("budget actions need a token estimate")
        })?;
        Ok(QuotaAction {
            policy_id: policy.quota_policy_id.clone(),
            environment_id: self.environment_id.clone(),
            source: QuotaSource::BudgetLimit,
            algorithm: QuotaAlgorithm::FixedWindow,
            meter: QuotaMeter::Tokens,
            limit_value: policy.limit_amount,
            burst: None,
            amount,
            window_start: Some(window_start),
            window_end: Some(window_end),
        })
    }
}

/// The only v1 calendar boundaries: UTC midnight, Monday UTC midnight, and
/// the first UTC midnight of the month. Duration arithmetic is deliberately
/// avoided so future timezone support can correctly account for DST.
pub fn period_window(
    now: DateTime<Utc>,
    period: &str,
) -> Result<(DateTime<Utc>, DateTime<Utc>), ServiceError> {
    let day = now.date_naive();
    let start_day = match period {
        "daily" => day,
        "weekly" => day - Duration::days(i64::from(day.weekday().num_days_from_monday())),
        "monthly" => {
            NaiveDate::from_ymd_opt(day.year(), day.month(), 1).expect("first day is valid")
        }
        _ => {
            return Err(ServiceError::invalid_field(
                "period",
                "must be daily, weekly, or monthly",
            ))
        }
    };
    let start = DateTime::<Utc>::from_naive_utc_and_offset(
        start_day.and_hms_opt(0, 0, 0).expect("midnight is valid"),
        Utc,
    );
    let end = match period {
        "daily" => start + Duration::days(1),
        "weekly" => start + Duration::days(7),
        "monthly" => {
            let next = if start_day.month() == 12 {
                NaiveDate::from_ymd_opt(start_day.year() + 1, 1, 1)
            } else {
                NaiveDate::from_ymd_opt(start_day.year(), start_day.month() + 1, 1)
            }
            .expect("first day is valid");
            DateTime::<Utc>::from_naive_utc_and_offset(
                next.and_hms_opt(0, 0, 0).expect("midnight is valid"),
                Utc,
            )
        }
        _ => unreachable!(),
    };
    Ok((start, end))
}

fn validate_compiled(actions: &[QuotaAction]) -> Result<(), ServiceError> {
    let mut ids = HashSet::new();
    for action in actions {
        if action.policy_id.is_empty() || !ids.insert(action.policy_id.as_str()) {
            return Err(ServiceError::failed_precondition(
                "compiled quota policies require unique stable IDs",
            ));
        }
        if action.limit_value == 0 || action.limit_value > i64::MAX as u64 {
            return Err(ServiceError::invalid_field(
                "limit_value",
                "must be positive and fit PostgreSQL BIGINT",
            ));
        }
        if action.amount > i64::MAX as u64 {
            return Err(ServiceError::invalid_field(
                "reservation",
                "must fit PostgreSQL BIGINT",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::types::PgInterval;

    #[test]
    fn utc_period_windows_align_to_calendar_boundaries() {
        let now = DateTime::parse_from_rfc3339("2026-09-03T14:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let (daily_start, daily_end) = period_window(now, "daily").unwrap();
        assert_eq!(daily_start.to_rfc3339(), "2026-09-03T00:00:00+00:00");
        assert_eq!(daily_end.to_rfc3339(), "2026-09-04T00:00:00+00:00");
        let (weekly_start, weekly_end) = period_window(now, "weekly").unwrap();
        assert_eq!(weekly_start.to_rfc3339(), "2026-08-31T00:00:00+00:00");
        assert_eq!(weekly_end.to_rfc3339(), "2026-09-07T00:00:00+00:00");
        let (monthly_start, monthly_end) = period_window(now, "monthly").unwrap();
        assert_eq!(monthly_start.to_rfc3339(), "2026-09-01T00:00:00+00:00");
        assert_eq!(monthly_end.to_rfc3339(), "2026-10-01T00:00:00+00:00");
    }

    fn budget_policy(id: &str, scope: &str, scope_id: &str) -> BudgetPolicy {
        BudgetPolicy {
            quota_policy_id: format!("quota-{id}"),
            id: id.to_string(),
            environment_id: "test".to_string(),
            scope: scope.to_string(),
            scope_id: scope_id.to_string(),
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
            limit_amount: 100,
            enabled: true,
            created_at: Utc::now(),
        }
    }

    fn resolver_with_budget(policy: BudgetPolicy) -> QuotaPolicyResolver {
        let budgets = Arc::new(crate::budget::MemoryBudgetPolicyStore::new(
            "test".to_string(),
        ));
        budgets.insert(policy).unwrap();
        QuotaPolicyResolver::new(
            "test".to_string(),
            budgets,
            Arc::new(crate::ratelimit::MemoryRateLimitPolicyStore::new(
                "test".to_string(),
            )),
        )
    }

    #[tokio::test]
    async fn budgets_match_the_key_and_its_usage_pool() {
        let budgets = Arc::new(crate::budget::MemoryBudgetPolicyStore::new(
            "test".to_string(),
        ));
        budgets
            .insert(budget_policy("key", "api_key", "key-1"))
            .unwrap();
        budgets
            .insert(budget_policy("pool", "usage_pool", "pool-1"))
            .unwrap();
        let resolver = QuotaPolicyResolver::new(
            "test".to_string(),
            budgets,
            Arc::new(crate::ratelimit::MemoryRateLimitPolicyStore::new(
                "test".to_string(),
            )),
        );
        let actions = resolver
            .budget_actions(
                "key-1",
                Some("pool-1"),
                TokenReservation::Known(17),
                Utc::now(),
            )
            .await
            .unwrap();
        assert_eq!(actions.len(), 2);
        assert!(actions.iter().all(|action| action.amount == 17));
    }

    #[tokio::test]
    async fn policy_presence_checks_include_usage_pool_and_route_scopes() {
        let budgets = Arc::new(crate::budget::MemoryBudgetPolicyStore::new(
            "test".to_string(),
        ));
        budgets
            .insert(budget_policy("pool", "usage_pool", "pool-1"))
            .unwrap();
        let rates = Arc::new(crate::ratelimit::MemoryRateLimitPolicyStore::new(
            "test".to_string(),
        ));
        rates
            .create(crate::ratelimit::RateLimitPolicy {
                quota_policy_id: "quota-pool-route".to_string(),
                id: "pool-route".to_string(),
                limit_type: "requests_per_minute".to_string(),
                limit_value: 100,
                burst_value: None,
                connection_id: Some("connection-1".to_string()),
                models: vec!["openai/gpt-5".to_string()],
                scope: "usage_pool".to_string(),
                scope_id: "pool-1".to_string(),
                enabled: true,
                created_at: Utc::now(),
            })
            .await
            .unwrap();
        let resolver = QuotaPolicyResolver::new("test".to_string(), budgets, rates);

        assert!(resolver
            .has_request_policies("key-1", Some("pool-1"))
            .await
            .unwrap());
        assert!(!resolver
            .has_request_policies("key-1", Some("other-pool"))
            .await
            .unwrap());
        let route = RateLimitMatch {
            key_hash: "key-1",
            usage_pool_id: Some("pool-1"),
            connection_id: Some("connection-1"),
            model: RateLimitModel::Exact("openai/gpt-5"),
        };
        assert!(resolver.has_rate_limit_policies(&route).await.unwrap());
    }

    #[tokio::test]
    async fn rolling_and_cost_budgets_are_rejected_explicitly() {
        let mut rolling = budget_policy("rolling", "api_key", "key-1");
        rolling.window_type = "rolling".to_string();
        let error = resolver_with_budget(rolling)
            .budget_actions("key-1", None, TokenReservation::Known(1), Utc::now())
            .await
            .expect_err("rolling budgets are unsupported");
        assert!(error.to_string().contains("rolling"));

        let mut cost = budget_policy("cost", "api_key", "key-1");
        cost.limit_kind = "cost_micro_usd".to_string();
        let error = resolver_with_budget(cost)
            .budget_actions("key-1", None, TokenReservation::Known(1), Utc::now())
            .await
            .expect_err("cost budgets are unsupported");
        assert!(error.to_string().contains("cost"));
    }

    #[tokio::test]
    async fn request_only_admission_omits_token_policies() {
        let budgets = Arc::new(crate::budget::MemoryBudgetPolicyStore::new(
            "test".to_string(),
        ));
        budgets
            .insert(budget_policy("key", "api_key", "key-1"))
            .unwrap();
        let rates = Arc::new(crate::ratelimit::MemoryRateLimitPolicyStore::new(
            "test".to_string(),
        ));
        for (id, limit_type) in [
            ("rpm", "requests_per_minute"),
            ("tpm", "tokens_per_minute"),
            ("concurrency", "concurrent_requests"),
        ] {
            rates
                .create(crate::ratelimit::RateLimitPolicy {
                    quota_policy_id: format!("quota-{id}"),
                    id: id.to_string(),
                    limit_type: limit_type.to_string(),
                    limit_value: 100,
                    burst_value: None,
                    connection_id: None,
                    models: Vec::new(),
                    scope: "api_key".to_string(),
                    scope_id: "key-1".to_string(),
                    enabled: true,
                    created_at: Utc::now(),
                })
                .await
                .unwrap();
        }
        let resolver = QuotaPolicyResolver::new("test".to_string(), budgets, rates);

        let plan = resolver
            .request_actions("key-1", None, TokenReservation::NotApplicable, Utc::now())
            .await
            .unwrap();

        assert!(!plan.requires_token_reservation);
        assert_eq!(plan.actions.len(), 2);
        assert!(plan.actions.iter().all(|action| {
            matches!(
                action.meter,
                QuotaMeter::Requests | QuotaMeter::ConcurrentRequests
            )
        }));
    }

    #[tokio::test]
    async fn provider_selected_model_fails_closed_only_for_applicable_model_limits() {
        let budgets = Arc::new(crate::budget::MemoryBudgetPolicyStore::new(
            "test".to_string(),
        ));
        let rates = Arc::new(crate::ratelimit::MemoryRateLimitPolicyStore::new(
            "test".to_string(),
        ));
        for (id, limit_type, models) in [
            ("connection-rpm", "requests_per_minute", Vec::new()),
            (
                "model-rpm",
                "requests_per_minute",
                vec!["openai/gpt-image-1".to_string()],
            ),
            (
                "model-tpm",
                "tokens_per_minute",
                vec!["openai/gpt-image-1".to_string()],
            ),
        ] {
            rates
                .create(crate::ratelimit::RateLimitPolicy {
                    quota_policy_id: format!("quota-{id}"),
                    id: id.to_string(),
                    limit_type: limit_type.to_string(),
                    limit_value: 100,
                    burst_value: Some(100),
                    connection_id: Some("openai".to_string()),
                    models,
                    scope: "api_key".to_string(),
                    scope_id: "key-1".to_string(),
                    enabled: true,
                    created_at: Utc::now(),
                })
                .await
                .unwrap();
        }
        let resolver = QuotaPolicyResolver::new("test".to_string(), budgets, rates.clone());
        let request = RateLimitMatch {
            key_hash: "key-1",
            usage_pool_id: None,
            connection_id: Some("openai"),
            model: RateLimitModel::ProviderSelected,
        };

        let plan = resolver
            .rate_actions(&request, TokenReservation::NotApplicable)
            .await
            .unwrap();
        assert!(plan.requires_public_model);
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.actions[0].policy_id, "quota-connection-rpm");

        rates.delete("model-rpm").await.unwrap();
        let plan = resolver
            .rate_actions(&request, TokenReservation::NotApplicable)
            .await
            .unwrap();
        assert!(!plan.requires_public_model);
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.actions[0].policy_id, "quota-connection-rpm");
    }

    #[tokio::test]
    async fn unknown_admission_requires_tokens_without_compiling_zero_token_actions() {
        let budgets = Arc::new(crate::budget::MemoryBudgetPolicyStore::new(
            "test".to_string(),
        ));
        budgets
            .insert(budget_policy("key", "api_key", "key-1"))
            .unwrap();
        let rates = Arc::new(crate::ratelimit::MemoryRateLimitPolicyStore::new(
            "test".to_string(),
        ));
        for (id, limit_type) in [("rpm", "requests_per_minute"), ("tpm", "tokens_per_minute")] {
            rates
                .create(crate::ratelimit::RateLimitPolicy {
                    quota_policy_id: format!("quota-{id}"),
                    id: id.to_string(),
                    limit_type: limit_type.to_string(),
                    limit_value: 100,
                    burst_value: None,
                    connection_id: None,
                    models: Vec::new(),
                    scope: "api_key".to_string(),
                    scope_id: "key-1".to_string(),
                    enabled: true,
                    created_at: Utc::now(),
                })
                .await
                .unwrap();
        }
        let resolver = QuotaPolicyResolver::new("test".to_string(), budgets, rates);

        let plan = resolver
            .request_actions("key-1", None, TokenReservation::Unknown, Utc::now())
            .await
            .unwrap();

        assert!(plan.requires_token_reservation);
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.actions[0].meter, QuotaMeter::Requests);
        assert!(plan
            .actions
            .iter()
            .all(|action| action.meter != QuotaMeter::Tokens));
    }

    #[tokio::test]
    async fn hosted_reservations_hold_a_clamped_reserve_per_policy() {
        use crate::quota::SOFT_CAP_RESERVE_TOKENS;

        let budgets = Arc::new(crate::budget::MemoryBudgetPolicyStore::new(
            "test".to_string(),
        ));
        budgets
            .insert(budget_policy("key", "api_key", "key-1"))
            .unwrap();
        let rates = Arc::new(crate::ratelimit::MemoryRateLimitPolicyStore::new(
            "test".to_string(),
        ));
        rates
            .create(crate::ratelimit::RateLimitPolicy {
                quota_policy_id: "quota-tpm".to_string(),
                id: "tpm".to_string(),
                limit_type: "tokens_per_minute".to_string(),
                limit_value: 1_000_000,
                burst_value: None,
                connection_id: None,
                models: Vec::new(),
                scope: "api_key".to_string(),
                scope_id: "key-1".to_string(),
                enabled: true,
                created_at: Utc::now(),
            })
            .await
            .unwrap();
        let resolver = QuotaPolicyResolver::new("test".to_string(), budgets, rates);

        let plan = resolver
            .request_actions("key-1", None, TokenReservation::SoftCap(40), Utc::now())
            .await
            .unwrap();

        assert!(!plan.requires_token_reservation);
        let amount = |policy: &str| {
            plan.actions
                .iter()
                .find(|action| action.policy_id == policy)
                .map(|action| action.amount)
        };
        // The 100-token budget clamps the reserve; the large TPM holds it all.
        assert_eq!(amount("quota-key"), Some(100));
        assert_eq!(amount("quota-tpm"), Some(40 + SOFT_CAP_RESERVE_TOKENS));
    }

    #[tokio::test]
    async fn hosted_reserves_throttle_parallel_admissions() {
        use crate::quota::{
            MemoryQuota, QuotaBackend, QuotaDecision, QuotaOutcome, SOFT_CAP_RESERVE_TOKENS,
        };

        let resolver = resolver_with_budget(BudgetPolicy {
            limit_amount: 2 * SOFT_CAP_RESERVE_TOKENS + 1_000,
            ..budget_policy("key", "api_key", "key-1")
        });
        let actions = resolver
            .request_actions("key-1", None, TokenReservation::SoftCap(100), Utc::now())
            .await
            .unwrap()
            .actions;
        let backend = MemoryQuota::new();
        let admit = || async { backend.acquire_many(&actions, None).await.unwrap() };

        let QuotaDecision::Allowed(Some(first)) = admit().await else {
            panic!("first hosted request fits");
        };
        assert!(matches!(admit().await, QuotaDecision::Allowed(Some(_))));
        // Both in-flight reserves hold the headroom before either settles.
        assert!(matches!(admit().await, QuotaDecision::Limited(_)));

        backend
            .finish(&first, QuotaOutcome::Actual(50))
            .await
            .unwrap();
        assert!(matches!(admit().await, QuotaDecision::Allowed(Some(_))));
    }
}
