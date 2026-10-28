use tonic::{Request, Response, Status};

use crate::AppState;

use super::conv;
use super::proto;
use super::proto::debug_service_server::DebugService;

pub struct DebugServer {
    pub state: AppState,
}

#[tonic::async_trait]
impl DebugService for DebugServer {
    async fn recent_completions(
        &self,
        _req: Request<proto::RecentCompletionsRequest>,
    ) -> Result<Response<proto::RecentCompletionsResponse>, Status> {
        let state = self.state.for_request(&_req);
        let entries = state.debug_service().recent_completions().await;
        let count = entries.len() as u32;
        Ok(Response::new(proto::RecentCompletionsResponse {
            count,
            entries: entries.iter().map(conv::debug_entry_to_proto).collect(),
        }))
    }
}
