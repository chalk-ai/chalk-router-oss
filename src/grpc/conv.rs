//! Conversions between domain types and proto messages.

use std::collections::HashMap;

use chrono::{DateTime, TimeZone, Utc};
use prost_types::{Duration, Timestamp};

use crate::api_keys::{ApiKey, ApiKeyConnectionScope, ApiKeyPermission, UsageSnapshot};
use crate::budget::{ApiKeyBudgetInput, BudgetPolicy};
use crate::prompt_cache::{CachePolicy, CacheScope};
use crate::ratelimit::{ApiKeyRateLimitInput, RateLimitPolicy};
use crate::service::api_keys::{ApiKeyPermissionInput, ApiKeyRouteRef, KeyUsageView};
use crate::usage_pools::UsagePool;
use crate::usage_tracker::UsageHistogramRow;
use crate::DebugCompletionEntry;

use super::proto;

pub fn ts_to_proto(t: DateTime<Utc>) -> Timestamp {
    Timestamp {
        seconds: t.timestamp(),
        nanos: t.timestamp_subsec_nanos() as i32,
    }
}

pub fn ts_from_proto(t: &Timestamp) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(t.seconds, t.nanos as u32).single()
}

/// Convert a non-negative millisecond count to a protobuf Duration.
///
/// `std::time::Duration -> prost_types::Duration` only fails if the second
/// count overflows `i64`, i.e. beyond ~292 billion years — unreachable for any
/// `u64` millisecond value the router actually produces (probe latencies,
/// completion durations), so a conversion failure here indicates a caller bug
/// rather than a normal runtime condition worth degrading silently to zero.
pub fn duration_from_millis(ms: u64) -> Duration {
    Duration::try_from(std::time::Duration::from_millis(ms))
        .expect("millisecond duration always fits in a protobuf Duration")
}

pub fn cache_scope_to_proto(scope: CacheScope) -> proto::CacheScope {
    match scope {
        CacheScope::Model => proto::CacheScope::Model,
        CacheScope::Team => proto::CacheScope::Team,
        CacheScope::Token => proto::CacheScope::Token,
    }
}

/// `UNSPECIFIED` — which is also what an unknown future variant decodes to —
/// maps to the narrowest scope, so a client that omits the field (or a rolled-
/// back server that doesn't know the variant) can never widen who a cached
/// completion is disclosed to.
pub fn cache_scope_from_proto(scope: i32) -> CacheScope {
    match proto::CacheScope::try_from(scope) {
        Ok(proto::CacheScope::Model) => CacheScope::Model,
        Ok(proto::CacheScope::Team) => CacheScope::Team,
        Ok(proto::CacheScope::Token) | Ok(proto::CacheScope::Unspecified) | Err(_) => {
            CacheScope::Token
        }
    }
}

pub fn cache_policy_to_proto(policy: &CachePolicy) -> proto::CachePolicy {
    proto::CachePolicy {
        enabled: policy.enabled,
        scope: cache_scope_to_proto(policy.scope) as i32,
        ttl_secs: policy.ttl_secs,
        max_entry_bytes: policy.max_entry_bytes,
    }
}

pub fn cache_policy_from_proto(policy: proto::CachePolicy) -> CachePolicy {
    CachePolicy {
        enabled: policy.enabled,
        scope: cache_scope_from_proto(policy.scope),
        ttl_secs: policy.ttl_secs,
        max_entry_bytes: policy.max_entry_bytes,
    }
}

/// The admin view of a permission. Legacy fields are never returned: the
/// service restates them as connection scopes first, and anything left over is
/// dropped while `routes` still marks the key restricted, so a client that
/// saves the view back never widens the key. An unreadable permission becomes
/// the empty restricted state: no routes and no connection scopes.
pub fn permission_to_proto(p: &ApiKeyPermission) -> proto::ApiKeyPermission {
    #[allow(deprecated)]
    proto::ApiKeyPermission {
        provider: None,
        models: Vec::new(),
        connection_id: None,
        downgradable: p.downgradable,
        model_floor: p.model_floor.clone(),
        cache: p.cache.as_ref().map(cache_policy_to_proto),
        connection_scopes: p
            .connection_scopes
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|s| proto::ApiKeyConnectionScope {
                connection_id: s.connection_id,
                models: s.models,
            })
            .collect(),
        routes: p.is_restricted().then(|| proto::ApiKeyRouteScope {
            refs: p
                .route_ids
                .iter()
                .flatten()
                .map(|id| proto::ApiKeyRouteRef {
                    identifier: Some(proto::api_key_route_ref::Identifier::RouteId(id.clone())),
                })
                .collect(),
        }),
    }
}

/// Legacy fields are carried through so the service can reject them.
#[allow(deprecated)]
#[allow(clippy::result_large_err)]
pub fn permission_from_proto(
    p: Option<proto::ApiKeyPermission>,
) -> Result<ApiKeyPermissionInput, tonic::Status> {
    let Some(p) = p else {
        return Ok(ApiKeyPermissionInput::default());
    };
    let routes = p
        .routes
        .map(|routes| {
            routes
                .refs
                .into_iter()
                .enumerate()
                .map(|(index, route)| match route.identifier {
                    Some(proto::api_key_route_ref::Identifier::RouteId(id)) => {
                        Ok(ApiKeyRouteRef::RouteId(id))
                    }
                    Some(proto::api_key_route_ref::Identifier::PublicModelId(id)) => {
                        Ok(ApiKeyRouteRef::PublicModelId(id))
                    }
                    None => Err(tonic::Status::invalid_argument(format!(
                        "permission.routes.refs[{index}]: select route_id or public_model_id"
                    ))),
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    let mut input: ApiKeyPermissionInput = ApiKeyPermission {
        provider: p.provider,
        connection_id: p.connection_id,
        models: if p.models.is_empty() {
            None
        } else {
            Some(p.models)
        },
        downgradable: p.downgradable,
        model_floor: p.model_floor,
        cache: p.cache.map(cache_policy_from_proto),
        connection_scopes: if p.connection_scopes.is_empty() {
            None
        } else {
            Some(
                p.connection_scopes
                    .into_iter()
                    .map(|s| ApiKeyConnectionScope {
                        connection_id: s.connection_id,
                        models: s.models,
                    })
                    .collect(),
            )
        },
        route_ids: None,
        unreadable: false,
    }
    .into();
    input.routes = routes;
    Ok(input)
}

pub fn api_key_rate_limit_from_proto(p: proto::ApiKeyRateLimit) -> ApiKeyRateLimitInput {
    ApiKeyRateLimitInput {
        connection_id: p.connection_id,
        models: p.models,
        requests_per_minute: p.requests_per_minute,
        request_burst: p.request_burst,
        tokens_per_minute: p.tokens_per_minute,
        token_burst: p.token_burst,
        concurrent_requests: p.concurrent_requests,
        enabled: p.enabled.unwrap_or(true),
    }
}

#[allow(clippy::result_large_err)] // tonic::Status is the gRPC wire error
pub fn api_key_budget_from_proto(
    p: proto::UsageBudgetSpec,
) -> Result<ApiKeyBudgetInput, tonic::Status> {
    let period = match proto::UsageBudgetPeriod::try_from(p.period)
        .unwrap_or(proto::UsageBudgetPeriod::Unspecified)
    {
        proto::UsageBudgetPeriod::Daily => "daily",
        proto::UsageBudgetPeriod::Weekly => "weekly",
        proto::UsageBudgetPeriod::Monthly => "monthly",
        proto::UsageBudgetPeriod::Unspecified => {
            return Err(tonic::Status::invalid_argument(
                "usage_budgets.period must be DAILY, WEEKLY, or MONTHLY",
            ));
        }
    };
    Ok(ApiKeyBudgetInput {
        token_limit: p.token_limit,
        period: period.to_string(),
        enabled: p.enabled.unwrap_or(true),
    })
}

pub fn usage_to_proto(u: &UsageSnapshot) -> proto::UsageSnapshot {
    proto::UsageSnapshot {
        prompt_tokens: u.prompt_tokens,
        completion_tokens: u.completion_tokens,
        total_tokens: u.total_tokens,
        request_count: u.request_count,
    }
}

#[allow(deprecated)] // Keep the reserved cost_tags response field empty.
pub fn api_key_with_usage_to_proto(
    k: &ApiKey,
    usage: &UsageSnapshot,
    rate_limits: &[ApiKeyRateLimitInput],
    usage_budgets: &[BudgetPolicy],
) -> proto::ApiKey {
    proto::ApiKey {
        key_hash: k.key_hash.clone(),
        key_prefix: k.key_prefix.clone(),
        description: k.description.clone(),
        created_at: Some(ts_to_proto(k.created_at)),
        permission: Some(permission_to_proto(&k.permission)),
        labels: k.labels.clone(),
        usage: Some(usage_to_proto(usage)),
        daily_token_budget: usage_budgets
            .iter()
            .find(|budget| budget.period == "daily")
            .map(|budget| budget.limit_amount)
            .or(k.daily_token_budget),
        cost_tags: HashMap::new(),
        usage_pool_id: k.usage_pool_id.clone(),
        rate_limits: rate_limits
            .iter()
            .map(|limit| proto::ApiKeyRateLimit {
                connection_id: limit.connection_id.clone(),
                models: limit.models.clone(),
                requests_per_minute: limit.requests_per_minute,
                request_burst: limit.request_burst,
                tokens_per_minute: limit.tokens_per_minute,
                token_burst: limit.token_burst,
                concurrent_requests: limit.concurrent_requests,
                enabled: Some(limit.enabled),
            })
            .collect(),
        usage_budgets: usage_budgets
            .iter()
            .map(|budget| proto::UsageBudget {
                id: budget.id.clone(),
                scope: budget.scope.clone(),
                scope_id: budget.scope_id.clone(),
                daily_token_budget: if budget.period == "daily" {
                    budget.limit_amount
                } else {
                    0
                },
                token_limit: budget.limit_amount,
                period: match budget.period.as_str() {
                    "weekly" => proto::UsageBudgetPeriod::Weekly as i32,
                    "monthly" => proto::UsageBudgetPeriod::Monthly as i32,
                    _ => proto::UsageBudgetPeriod::Daily as i32,
                },
                enabled: budget.enabled,
                created_at: Some(ts_to_proto(budget.created_at)),
                // API-key reads expose control-plane definitions. Runtime
                // counters are surfaced by UsageBudgetService instead.
                current_usage: None,
                next_reset_at: None,
            })
            .collect(),
    }
}

pub fn key_usage_to_proto(v: &KeyUsageView) -> proto::GetKeyUsageResponse {
    proto::GetKeyUsageResponse {
        key: v.key.clone(),
        description: v.description.clone(),
        daily_token_budget: v.daily_token_budget,
        usage: Some(usage_to_proto(&v.usage)),
    }
}

pub fn usage_pool_to_proto(p: &UsagePool) -> proto::UsagePool {
    proto::UsagePool {
        id: p.id.clone(),
        name: p.name.clone(),
        created_at: Some(ts_to_proto(p.created_at)),
    }
}

pub fn rate_limit_to_proto(p: &RateLimitPolicy) -> proto::RateLimitPolicy {
    proto::RateLimitPolicy {
        id: p.id.clone(),
        limit_type: p.limit_type.clone(),
        limit_value: p.limit_value,
        scope: p.scope.clone(),
        scope_id: p.scope_id.clone(),
        enabled: p.enabled,
        created_at: Some(ts_to_proto(p.created_at)),
        burst_value: p.burst_value,
        connection_id: p.connection_id.clone(),
        models: p.models.clone(),
    }
}

pub fn sampling_policy_to_proto(
    p: &crate::sampling_policy::SamplingPolicy,
) -> proto::SamplingPolicy {
    proto::SamplingPolicy {
        enabled: p.enabled,
        default_rate: p.default_rate,
        default_log_content: p.default_log_content,
        rules: p.rules.iter().map(sampling_rule_to_proto).collect(),
        min_conversation_rate: p.min_conversation_rate,
        outcome_rules: p.outcome_rules.iter().map(outcome_rule_to_proto).collect(),
    }
}

pub fn outcome_rule_to_proto(r: &crate::sampling_policy::OutcomeRule) -> proto::OutcomeRule {
    use crate::sampling_policy::OutcomeComparator as Op;
    let op = match r.op {
        Op::Eq => proto::OutcomeComparator::Eq,
        Op::Ne => proto::OutcomeComparator::Ne,
        Op::Gt => proto::OutcomeComparator::Gt,
        Op::Lt => proto::OutcomeComparator::Lt,
        Op::Gte => proto::OutcomeComparator::Gte,
        Op::Lte => proto::OutcomeComparator::Lte,
    };
    proto::OutcomeRule {
        path: r.path.clone(),
        op: op as i32,
        value: Some(super::provider_connections::json_value_to_prost(
            r.value.clone(),
        )),
        rate: r.rate,
    }
}

/// A rule off the wire. Unlike [`sampling_rule_from_proto`] this is fallible:
/// `op` is a required, meaningful field with no safe default to fall back to
/// (defaulting an unspecified/invalid comparator to, say, `Eq` would silently
/// change what an operator's rule tests), so an unspecified or out-of-range
/// value is rejected rather than guessed.
#[allow(clippy::result_large_err)] // tonic::Status is the gRPC wire error
pub fn outcome_rule_from_proto(
    r: proto::OutcomeRule,
) -> Result<crate::sampling_policy::OutcomeRule, tonic::Status> {
    use crate::sampling_policy::OutcomeComparator as Op;
    let op = match proto::OutcomeComparator::try_from(r.op) {
        Ok(proto::OutcomeComparator::Eq) => Op::Eq,
        Ok(proto::OutcomeComparator::Ne) => Op::Ne,
        Ok(proto::OutcomeComparator::Gt) => Op::Gt,
        Ok(proto::OutcomeComparator::Lt) => Op::Lt,
        Ok(proto::OutcomeComparator::Gte) => Op::Gte,
        Ok(proto::OutcomeComparator::Lte) => Op::Lte,
        _ => {
            return Err(tonic::Status::invalid_argument(
                "outcome_rules[].op must be a valid OutcomeComparator",
            ))
        }
    };
    let value = r
        .value
        .map(super::provider_connections::prost_value_to_json)
        .unwrap_or(serde_json::Value::Null);
    crate::sampling_policy::validate_outcome_value(&value).map_err(|reason| {
        tonic::Status::invalid_argument(format!("outcome_rules[].value {reason}"))
    })?;
    Ok(crate::sampling_policy::OutcomeRule {
        path: r.path,
        op,
        value,
        rate: r.rate,
    })
}

pub fn sampling_rule_to_proto(r: &crate::sampling_policy::SamplingRule) -> proto::SamplingRule {
    use proto::sampling_rule::AppliesTo;

    let applies_to = match (&r.request_tag, &r.key_id, &r.usage_pool_id) {
        (Some(value), None, None) => Some(AppliesTo::RequestTag(value.clone())),
        (None, Some(value), None) => Some(AppliesTo::KeyId(value.clone())),
        (None, None, Some(value)) => Some(AppliesTo::UsagePoolId(value.clone())),
        _ => unreachable!("sampling policy rules have exactly one target"),
    };
    proto::SamplingRule {
        applies_to,
        rate: r.rate,
        log_content: r.log_content,
    }
}

/// A rule off the wire. A target must be selected and its value must not be
/// blank: treating either as unset would produce a catch-all rule or silently
/// change an operator's intended target.
#[allow(clippy::result_large_err)] // tonic::Status is the gRPC wire error
pub fn sampling_rule_from_proto(
    r: proto::SamplingRule,
) -> Result<crate::sampling_policy::SamplingRule, tonic::Status> {
    use proto::sampling_rule::AppliesTo;

    let target = r
        .applies_to
        .ok_or_else(|| tonic::Status::invalid_argument("rules[].applies_to must be set"))?;
    let value = match &target {
        AppliesTo::RequestTag(value) | AppliesTo::KeyId(value) | AppliesTo::UsagePoolId(value) => {
            value.trim()
        }
    };
    if value.is_empty() {
        return Err(tonic::Status::invalid_argument(
            "rules[].applies_to value must be non-empty",
        ));
    }

    let (request_tag, key_id, usage_pool_id) = match target {
        AppliesTo::RequestTag(value) => (Some(value.trim().to_string()), None, None),
        AppliesTo::KeyId(value) => (None, Some(value.trim().to_string()), None),
        AppliesTo::UsagePoolId(value) => (None, None, Some(value.trim().to_string())),
    };
    Ok(crate::sampling_policy::SamplingRule {
        request_tag,
        key_id,
        usage_pool_id,
        rate: r.rate,
        log_content: r.log_content,
    })
}

#[allow(deprecated)] // Wire compatibility: field 1 remains present but is always empty.
pub fn usage_histogram_row_to_proto(r: &UsageHistogramRow) -> proto::UsageHistogramRow {
    proto::UsageHistogramRow {
        provider: String::new(),
        bucket: Some(ts_to_proto(r.bucket)),
        total_tokens: r.total_tokens,
        connection_id: r.connection_id.clone().unwrap_or_default(),
        prompt_tokens: r.prompt_tokens,
        completion_tokens: r.completion_tokens,
        cached_tokens: r.cached_tokens,
        cache_write_tokens: r.cache_write_tokens,
        reasoning_tokens: r.reasoning_tokens,
        request_count: r.request_count,
    }
}

pub fn debug_entry_to_proto(e: &DebugCompletionEntry) -> proto::DebugCompletionEntry {
    let timestamp = match DateTime::parse_from_rfc3339(&e.timestamp) {
        Ok(timestamp) => Some(ts_to_proto(timestamp.with_timezone(&Utc))),
        Err(error) => {
            // Fabricating `Utc::now()` here would silently mislabel a debug
            // entry with the wrong time. Omit the field instead so a bad
            // upstream timestamp is visibly absent rather than plausibly wrong.
            tracing::warn!(
                timestamp = %e.timestamp,
                %error,
                "invalid debug completion timestamp; omitting timestamp field"
            );
            None
        }
    };
    proto::DebugCompletionEntry {
        timestamp,
        model: e.model.clone(),
        message_count: e.message_count as u32,
        streaming: e.streaming,
        status: e.status.clone(),
        duration: Some(duration_from_millis(e.duration_ms)),
        usage: e.usage.as_ref().map(|u| proto::Usage {
            prompt_tokens: u.prompt_tokens.max(0) as u32,
            completion_tokens: u.completion_tokens.unwrap_or(0).max(0) as u32,
            total_tokens: u.total_tokens.max(0) as u32,
        }),
        error: e.error.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(deprecated)]
    fn permission_responses_mark_restriction_with_routes() {
        let unrestricted = permission_to_proto(&ApiKeyPermission::default());
        assert_eq!(unrestricted.routes, None);

        // An unreadable permission is the empty restricted state, so saving it
        // back keeps the key denied.
        let unreadable = permission_to_proto(&ApiKeyPermission::unreadable());
        assert_eq!(
            unreadable.routes,
            Some(proto::ApiKeyRouteScope { refs: Vec::new() })
        );
        assert!(unreadable.connection_scopes.is_empty());
        assert!(permission_from_proto(Some(unreadable))
            .unwrap()
            .routes
            .unwrap()
            .is_empty());

        // A leftover legacy field is never returned, but the key still reads
        // as restricted.
        let legacy = permission_to_proto(&ApiKeyPermission {
            provider: Some("openai".into()),
            ..Default::default()
        });
        assert_eq!(legacy.provider, None);
        assert!(legacy.routes.is_some());

        let routed = ApiKeyPermission {
            route_ids: Some(vec!["route-a".into()]),
            ..Default::default()
        };
        assert_eq!(
            permission_to_proto(&routed).routes.unwrap().refs,
            vec![proto::ApiKeyRouteRef {
                identifier: Some(proto::api_key_route_ref::Identifier::RouteId(
                    "route-a".into()
                )),
            }]
        );
    }

    #[test]
    fn sampling_rule_proto_requires_one_nonblank_target() {
        let missing = sampling_rule_from_proto(proto::SamplingRule {
            applies_to: None,
            rate: 1.0,
            log_content: true,
        });
        assert!(missing.is_err());

        let blank = sampling_rule_from_proto(proto::SamplingRule {
            applies_to: Some(proto::sampling_rule::AppliesTo::KeyId("  ".to_string())),
            rate: 1.0,
            log_content: true,
        });
        assert!(blank.is_err());

        let rule = sampling_rule_from_proto(proto::SamplingRule {
            applies_to: Some(proto::sampling_rule::AppliesTo::UsagePoolId(
                " pool-internal ".to_string(),
            )),
            rate: 0.5,
            log_content: false,
        })
        .expect("oneof target is accepted");
        assert_eq!(rule.usage_pool_id.as_deref(), Some("pool-internal"));
        assert_eq!(rule.request_tag, None);
        assert_eq!(rule.key_id, None);
    }
}
