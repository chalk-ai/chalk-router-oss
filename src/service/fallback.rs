use std::collections::HashMap;
use std::sync::Arc;

use crate::fallback::{FallbackPolicy, FallbackPolicyStore};
use crate::service::ServiceError;

const MAX_POLICY_MODELS: usize = 1_000;
const MAX_FALLBACKS_PER_MODEL: usize = 100;
const MAX_MODEL_ID_BYTES: usize = 512;

pub struct FallbackPolicyService {
    store: Arc<dyn FallbackPolicyStore>,
}

impl FallbackPolicyService {
    pub fn new(store: Arc<dyn FallbackPolicyStore>) -> Self {
        Self { store }
    }

    /// Read the active policy. An `Err` means the store could not be read —
    /// admin surfaces report it rather than showing a misleading empty policy.
    pub async fn get(&self) -> Result<HashMap<String, Vec<String>>, ServiceError> {
        Ok(self.get_current().await?.as_map().clone())
    }

    /// Read the active policy including its store-managed `revision`, for the
    /// read-modify-write CAS flow behind partial (field-mask) updates. Prefer
    /// [`get`] when only the fallback map is needed.
    pub async fn get_current(&self) -> Result<FallbackPolicy, ServiceError> {
        self.store
            .get()
            .await
            .map_err(|e| ServiceError::internal(e.to_string()))
    }

    /// Persist a fallback policy update. An `Err` means the write did not
    /// durably land — callers must propagate the failure rather than
    /// reporting success (see `FallbackPolicyStore::set`).
    pub async fn set(
        &self,
        fallbacks: HashMap<String, Vec<String>>,
    ) -> Result<HashMap<String, Vec<String>>, ServiceError> {
        let expected = self.get_current().await?;
        self.set_cas(&expected, FallbackPolicy::from_map(fallbacks))
            .await
    }

    /// Validate and atomically replace a policy if it is still based on
    /// `expected`. This is the service-layer RMW primitive used by admin
    /// partial updates.
    pub async fn set_cas(
        &self,
        expected: &FallbackPolicy,
        policy: FallbackPolicy,
    ) -> Result<HashMap<String, Vec<String>>, ServiceError> {
        validate_size(policy.as_map())?;
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
        tracing::info!(revision = updated.revision, "Fallback policy updated");
        Ok(updated.as_map().clone())
    }
}

fn validate_size(fallbacks: &HashMap<String, Vec<String>>) -> Result<(), ServiceError> {
    if fallbacks.len() > MAX_POLICY_MODELS {
        return Err(ServiceError::invalid_argument(format!(
            "fallback policy exceeds the {MAX_POLICY_MODELS}-model limit"
        )));
    }
    for (model, alternatives) in fallbacks {
        if model.len() > MAX_MODEL_ID_BYTES
            || alternatives
                .iter()
                .any(|candidate| candidate.len() > MAX_MODEL_ID_BYTES)
        {
            return Err(ServiceError::invalid_argument(format!(
                "fallback policy model ids must be at most {MAX_MODEL_ID_BYTES} bytes"
            )));
        }
        if alternatives.len() > MAX_FALLBACKS_PER_MODEL {
            return Err(ServiceError::invalid_argument(format!(
                "fallback policy entry '{model}' exceeds the \
                 {MAX_FALLBACKS_PER_MODEL}-fallback limit"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversized_fallback_lists() {
        let mut policy = HashMap::new();
        policy.insert(
            "chalk/primary".to_string(),
            (0..=MAX_FALLBACKS_PER_MODEL)
                .map(|i| format!("chalk/fallback-{i}"))
                .collect(),
        );
        assert!(validate_size(&policy).is_err());
    }
}
