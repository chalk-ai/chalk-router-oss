use std::sync::Arc;

use crate::judge_policy::{JudgePolicy, JudgePolicyStore};
use crate::service::ServiceError;

const MAX_POLICY_MODELS: usize = 1_000;
const MAX_TARGETS_PER_LADDER: usize = 100;
const MAX_MODEL_ID_BYTES: usize = 512;

/// Wire-format-agnostic wrapper over the judge-policy store. Mirrors
/// [`crate::service::FallbackPolicyService`].
pub struct JudgePolicyService {
    store: Arc<dyn JudgePolicyStore>,
}

impl JudgePolicyService {
    pub fn new(store: Arc<dyn JudgePolicyStore>) -> Self {
        Self { store }
    }

    pub async fn get(&self) -> Result<JudgePolicy, ServiceError> {
        self.store
            .get()
            .await
            .map_err(|e| ServiceError::internal(e.to_string()))
    }

    /// Validate and persist. Malformed ladder boundaries are rejected here —
    /// once persisted they would silently mis-band every rung selection. A
    /// persistence failure is surfaced (500 / INTERNAL), never reported as
    /// success: an unpersisted policy would otherwise be served briefly by
    /// this replica, then silently revert at cache expiry — and never reach
    /// the others.
    pub async fn set(&self, policy: JudgePolicy) -> Result<JudgePolicy, ServiceError> {
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
        expected: &JudgePolicy,
        policy: JudgePolicy,
    ) -> Result<JudgePolicy, ServiceError> {
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
            shadow = updated.shadow,
            judge_model = %updated.judge_model,
            default_opt_in = updated.default_opt_in,
            revision = updated.revision,
            "Judge policy updated"
        );
        Ok(updated)
    }

    fn validate(&self, policy: &JudgePolicy) -> Result<(), ServiceError> {
        if policy.ladders.len() > MAX_POLICY_MODELS {
            return Err(ServiceError::invalid_argument(format!(
                "judge policy exceeds the {MAX_POLICY_MODELS}-model limit"
            )));
        }
        if policy.judge_model.len() > MAX_MODEL_ID_BYTES
            || policy
                .default_floor
                .as_ref()
                .is_some_and(|model| model.len() > MAX_MODEL_ID_BYTES)
        {
            return Err(ServiceError::invalid_argument(format!(
                "judge policy model ids must be at most {MAX_MODEL_ID_BYTES} bytes"
            )));
        }
        for (model, ladder) in &policy.ladders {
            if model.len() > MAX_MODEL_ID_BYTES
                || ladder
                    .targets
                    .iter()
                    .any(|target| target.len() > MAX_MODEL_ID_BYTES)
            {
                return Err(ServiceError::invalid_argument(format!(
                    "judge policy model ids must be at most {MAX_MODEL_ID_BYTES} bytes"
                )));
            }
            if ladder.targets.len() > MAX_TARGETS_PER_LADDER {
                return Err(ServiceError::invalid_argument(format!(
                    "judge ladder '{model}' exceeds the \
                     {MAX_TARGETS_PER_LADDER}-target limit"
                )));
            }
        }
        policy.validate().map_err(ServiceError::invalid_argument)
    }
}
