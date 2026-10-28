//! Retired global policy API. Configuration now belongs to model routes.
use super::proto;
use super::proto::judge_policy_service_server::JudgePolicyService;
use tonic::{Request, Response, Status};
pub struct JudgePolicyServer;
#[tonic::async_trait]
impl JudgePolicyService for JudgePolicyServer {
    async fn get_judge_policy(
        &self,
        _: Request<proto::GetJudgePolicyRequest>,
    ) -> Result<Response<proto::GetJudgePolicyResponse>, Status> {
        Err(Status::failed_precondition(
            "Global judge policies are retired; configure ModelRoute.routing",
        ))
    }
    async fn update_judge_policy(
        &self,
        _: Request<proto::UpdateJudgePolicyRequest>,
    ) -> Result<Response<proto::UpdateJudgePolicyResponse>, Status> {
        Err(Status::failed_precondition(
            "Global judge policies are retired; configure ModelRoute.routing",
        ))
    }
}
