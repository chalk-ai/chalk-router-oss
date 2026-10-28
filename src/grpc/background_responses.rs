use tonic::{Request, Response, Status};

use super::{conv::ts_to_proto, proto};
use crate::AppState;
use proto::background_response_service_server::BackgroundResponseService;

pub struct BackgroundResponseServer {
    pub state: AppState,
}

#[tonic::async_trait]
impl BackgroundResponseService for BackgroundResponseServer {
    async fn list_background_responses(
        &self,
        request: Request<proto::ListBackgroundResponsesRequest>,
    ) -> Result<Response<proto::ListBackgroundResponsesResponse>, Status> {
        let state = self.state.for_request(&request);
        let request = request.into_inner();
        let service = state.background_response_service();
        let page = service
            .list(
                request.phase.as_deref(),
                request.cursor.as_deref(),
                request.limit,
            )
            .await?;
        Ok(Response::new(proto::ListBackgroundResponsesResponse {
            jobs: page
                .jobs
                .into_iter()
                .map(|job| proto::BackgroundResponseJob {
                    event_id: job.event_id.to_string(),
                    phase: job.phase,
                    connection_id: job.connection_id,
                    upstream_response_id: job.upstream_response_id,
                    requested_model: job.requested_model,
                    served_model: job.served_model,
                    upstream_status: job.upstream_status,
                    submitted_at: Some(ts_to_proto(job.submitted_at)),
                    terminal_at: job.terminal_at.map(ts_to_proto),
                    settled_at: job.settled_at.map(ts_to_proto),
                    next_poll_at: Some(ts_to_proto(job.next_poll_at)),
                    claim_until: job.claim_until.map(ts_to_proto),
                    poll_attempts: job.poll_attempts,
                    last_error_code: job.last_error_code,
                })
                .collect(),
            next_cursor: page.next_cursor,
        }))
    }
}
