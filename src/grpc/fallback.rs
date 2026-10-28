//! Retired global policy API. Configuration now belongs to model routes.
use super::proto;
use super::proto::fallback_policy_service_server::FallbackPolicyService;
use tonic::{Request, Response, Status};
pub struct FallbackPolicyServer;
#[tonic::async_trait]
impl FallbackPolicyService for FallbackPolicyServer {
    async fn get_fallback_policy(
        &self,
        _: Request<proto::GetFallbackPolicyRequest>,
    ) -> Result<Response<proto::GetFallbackPolicyResponse>, Status> {
        Err(Status::failed_precondition(
            "Global fallback policies are retired; configure ModelRoute.routing",
        ))
    }
    async fn update_fallback_policy(
        &self,
        _: Request<proto::UpdateFallbackPolicyRequest>,
    ) -> Result<Response<proto::UpdateFallbackPolicyResponse>, Status> {
        Err(Status::failed_precondition(
            "Global fallback policies are retired; configure ModelRoute.routing",
        ))
    }
}
