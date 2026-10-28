use std::sync::Arc;

use crate::sampling_policy::{SamplingPolicy, SamplingPolicyStore};
use crate::service::ServiceError;

/// Bound on the rule list. Every replica re-decodes this document on each
/// policy refresh and walks it per request, so an unbounded list would let one
/// admin edit turn the request path into a linear scan of arbitrary length.
const MAX_SAMPLING_RULES: usize = 1_000;

/// Bound on each condition string. Request tags are freeform and minted by
/// callers, so a rule may be written against an arbitrary one; the same bound
/// covers key ids and pool ids, which are shorter in practice.
const MAX_RULE_CONDITION_BYTES: usize = 256;

/// Bound on the outcome-rule list, for the same reason as
/// `MAX_SAMPLING_RULES`: every response completion re-checks this list.
const MAX_OUTCOME_RULES: usize = 1_000;

/// Bound on an outcome rule's `path`. Response bodies can nest deeply, but an
/// operator-authored path has no reason to run unbounded.
const MAX_OUTCOME_PATH_BYTES: usize = 256;

/// Wire-format-agnostic wrapper over the sampling-policy store. Mirrors
/// [`crate::service::JudgePolicyService`].
pub struct SamplingPolicyService {
    store: Arc<dyn SamplingPolicyStore>,
}

impl SamplingPolicyService {
    pub fn new(store: Arc<dyn SamplingPolicyStore>) -> Self {
        Self { store }
    }

    pub async fn get(&self) -> Result<SamplingPolicy, ServiceError> {
        self.store
            .get()
            .await
            .map_err(|e| ServiceError::internal(e.to_string()))
    }

    /// Validate and persist. A persistence failure is surfaced (500 /
    /// INTERNAL), never reported as success: an unpersisted policy would
    /// otherwise be served briefly by this replica, then silently revert at
    /// cache expiry — and never reach the others.
    pub async fn set(&self, policy: SamplingPolicy) -> Result<SamplingPolicy, ServiceError> {
        let expected = self
            .store
            .get()
            .await
            .map_err(|e| ServiceError::internal(e.to_string()))?;
        self.set_cas(&expected, policy).await
    }

    /// Validate and atomically replace a policy if it is still based on
    /// `expected`. This is the service-layer RMW primitive used by admin
    /// partial updates.
    pub async fn set_cas(
        &self,
        expected: &SamplingPolicy,
        policy: SamplingPolicy,
    ) -> Result<SamplingPolicy, ServiceError> {
        self.validate(&policy)?;
        let mut updated = policy;
        updated.revision = expected.revision.saturating_add(1);
        self.store
            .set_cas(expected, updated.clone())
            .await
            .map_err(|e| {
                if e.is_conflict() {
                    ServiceError::failed_precondition(e.to_string())
                } else {
                    ServiceError::internal(e.to_string())
                }
            })?;
        tracing::info!(
            enabled = updated.enabled,
            default_rate = updated.default_rate,
            rules = updated.rules.len(),
            default_log_content = updated.default_log_content,
            min_conversation_rate = updated.min_conversation_rate,
            revision = updated.revision,
            "Sampling policy updated"
        );
        Ok(updated)
    }

    fn validate(&self, policy: &SamplingPolicy) -> Result<(), ServiceError> {
        if policy.rules.len() > MAX_SAMPLING_RULES {
            return Err(ServiceError::invalid_argument(format!(
                "sampling policy exceeds the {MAX_SAMPLING_RULES}-rule limit"
            )));
        }
        for (index, rule) in policy.rules.iter().enumerate() {
            for (field, condition) in [
                ("request_tag", &rule.request_tag),
                ("key_id", &rule.key_id),
                ("usage_pool_id", &rule.usage_pool_id),
            ] {
                let Some(condition) = condition else {
                    continue;
                };
                // A blank condition is rejected rather than dropped: dropping it
                // would widen the rule to match more than the operator wrote,
                // and dropping the last one would make it match everything.
                if condition.trim().is_empty() {
                    return Err(ServiceError::invalid_argument(format!(
                        "rules[{index}].{field} must be non-empty when set"
                    )));
                }
                if condition.len() > MAX_RULE_CONDITION_BYTES {
                    return Err(ServiceError::invalid_argument(format!(
                        "rules[{index}].{field} must be at most \
                         {MAX_RULE_CONDITION_BYTES} bytes"
                    )));
                }
            }
        }
        if policy.outcome_rules.len() > MAX_OUTCOME_RULES {
            return Err(ServiceError::invalid_argument(format!(
                "sampling policy exceeds the {MAX_OUTCOME_RULES}-outcome-rule limit"
            )));
        }
        for (index, rule) in policy.outcome_rules.iter().enumerate() {
            if rule.path.len() > MAX_OUTCOME_PATH_BYTES {
                return Err(ServiceError::invalid_argument(format!(
                    "outcome_rules[{index}].path must be at most \
                     {MAX_OUTCOME_PATH_BYTES} bytes"
                )));
            }
        }
        policy.validate().map_err(ServiceError::invalid_argument)
    }
}
