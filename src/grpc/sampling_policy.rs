use fieldmask::{Maskable, SelfMaskable};
use tonic::{Request, Response, Status};

use crate::sampling_policy::{OutcomeRule, SamplingPolicy, SamplingRule};
use crate::AppState;

use super::conv;
use super::field_mask;
use super::proto;
use super::proto::sampling_policy_service_server::SamplingPolicyService;

pub struct SamplingPolicyServer {
    pub state: AppState,
}

/// Updatable sampling-policy fields (revision is store-managed, not on the wire).
///
/// `rules` is whole-list replacement: [`field_mask`]'s update options set
/// `replace_repeated`, and rule *order* is the policy's precedence, so merging
/// two lists element-wise would silently reorder an operator's rules.
#[derive(Debug, Clone, Default, Maskable, SelfMaskable)]
struct SamplingPolicyPatch {
    enabled: bool,
    default_rate: f64,
    default_log_content: bool,
    rules: Vec<SamplingRule>,
    min_conversation_rate: f64,
    outcome_rules: Vec<OutcomeRule>,
}

impl From<&SamplingPolicy> for SamplingPolicyPatch {
    fn from(p: &SamplingPolicy) -> Self {
        Self {
            enabled: p.enabled,
            default_rate: p.default_rate,
            default_log_content: p.default_log_content,
            rules: p.rules.clone(),
            min_conversation_rate: p.min_conversation_rate,
            outcome_rules: p.outcome_rules.clone(),
        }
    }
}

impl SamplingPolicyPatch {
    fn write_into(self, policy: &mut SamplingPolicy) {
        policy.enabled = self.enabled;
        policy.default_rate = self.default_rate;
        policy.default_log_content = self.default_log_content;
        policy.rules = self.rules;
        policy.min_conversation_rate = self.min_conversation_rate;
        policy.outcome_rules = self.outcome_rules;
    }

    #[allow(clippy::result_large_err)] // tonic::Status is the gRPC wire error
    fn from_operation(upd: &proto::UpdateSamplingPolicyOperation) -> Result<Self, Status> {
        Ok(Self {
            enabled: upd.enabled.unwrap_or_default(),
            default_rate: upd.default_rate.unwrap_or_default(),
            default_log_content: upd.default_log_content.unwrap_or_default(),
            rules: upd
                .rules
                .iter()
                .cloned()
                .map(conv::sampling_rule_from_proto)
                .collect::<Result<Vec<_>, Status>>()?,
            min_conversation_rate: upd.min_conversation_rate.unwrap_or_default(),
            outcome_rules: upd
                .outcome_rules
                .iter()
                .cloned()
                .map(conv::outcome_rule_from_proto)
                .collect::<Result<Vec<_>, Status>>()?,
        })
    }
}

#[tonic::async_trait]
impl SamplingPolicyService for SamplingPolicyServer {
    async fn get_sampling_policy(
        &self,
        _req: Request<proto::GetSamplingPolicyRequest>,
    ) -> Result<Response<proto::GetSamplingPolicyResponse>, Status> {
        let state = self.state.for_request(&_req);
        let policy = state
            .sampling_service()
            .get()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(proto::GetSamplingPolicyResponse {
            policy: Some(conv::sampling_policy_to_proto(&policy)),
        }))
    }

    async fn update_sampling_policy(
        &self,
        req: Request<proto::UpdateSamplingPolicyRequest>,
    ) -> Result<Response<proto::UpdateSamplingPolicyResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        let upd = body.update.unwrap_or_default();
        let mut populated = Vec::new();
        if upd.enabled.is_some() {
            populated.push("enabled");
        }
        if upd.default_rate.is_some() {
            populated.push("default_rate");
        }
        if !upd.rules.is_empty() {
            populated.push("rules");
        }
        if upd.default_log_content.is_some() {
            populated.push("default_log_content");
        }
        if upd.min_conversation_rate.is_some() {
            populated.push("min_conversation_rate");
        }
        if !upd.outcome_rules.is_empty() {
            populated.push("outcome_rules");
        }
        let resolved = field_mask::resolve(&body.update_mask, &populated)?;

        let mut policy = state
            .sampling_service()
            .get()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        if resolved.is_noop() {
            return Ok(Response::new(proto::UpdateSamplingPolicyResponse {
                policy: Some(conv::sampling_policy_to_proto(&policy)),
            }));
        }

        let expected = policy.clone();
        let source = SamplingPolicyPatch::from_operation(&upd)?;
        let mut target = SamplingPolicyPatch::from(&policy);
        field_mask::apply(&resolved, &mut target, source)?;
        target.write_into(&mut policy);

        let updated = state
            .sampling_service()
            .set_cas(&expected, policy)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(proto::UpdateSamplingPolicyResponse {
            policy: Some(conv::sampling_policy_to_proto(&updated)),
        }))
    }
}
