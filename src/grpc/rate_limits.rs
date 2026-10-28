use fieldmask::{Maskable, SelfMaskable};
use tonic::{Request, Response, Status};

use crate::service::rate_limits::{CreateRateLimitInput, UpdateRateLimitInput};
use crate::AppState;

use super::conv;
use super::field_mask;
use super::proto;
use super::proto::rate_limit_service_server::RateLimitService;

pub struct RateLimitServer {
    pub state: AppState,
}

#[derive(Debug, Default, Maskable, SelfMaskable)]
struct RateLimitPatch {
    enabled: bool,
    limit_type: String,
    limit_value: u64,
    burst_value: Option<u64>,
    connection_id: Option<String>,
    models: Vec<String>,
}

#[tonic::async_trait]
impl RateLimitService for RateLimitServer {
    async fn create_rate_limit(
        &self,
        req: Request<proto::CreateRateLimitRequest>,
    ) -> Result<Response<proto::CreateRateLimitResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        let policy = state
            .rate_limit_service()
            .create(CreateRateLimitInput {
                limit_type: body.limit_type,
                limit_value: body.limit_value,
                burst_value: body.burst_value,
                connection_id: body.connection_id,
                models: body.models,
                scope: body.scope,
                scope_id: body.scope_id,
                enabled: body.enabled.unwrap_or(true),
            })
            .await?;
        Ok(Response::new(proto::CreateRateLimitResponse {
            policy: Some(conv::rate_limit_to_proto(&policy)),
        }))
    }

    async fn list_rate_limits(
        &self,
        req: Request<proto::ListRateLimitsRequest>,
    ) -> Result<Response<proto::ListRateLimitsResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        let (policies, next_cursor) = state
            .rate_limit_service()
            .list_page(body.limit, body.cursor)
            .await?;
        Ok(Response::new(proto::ListRateLimitsResponse {
            policies: policies.iter().map(conv::rate_limit_to_proto).collect(),
            next_cursor,
        }))
    }

    async fn get_rate_limit(
        &self,
        req: Request<proto::GetRateLimitRequest>,
    ) -> Result<Response<proto::GetRateLimitResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        let policy = state.rate_limit_service().get(&body.id).await?;
        Ok(Response::new(proto::GetRateLimitResponse {
            policy: Some(conv::rate_limit_to_proto(&policy)),
        }))
    }

    async fn update_rate_limit(
        &self,
        req: Request<proto::UpdateRateLimitRequest>,
    ) -> Result<Response<proto::UpdateRateLimitResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        let upd = body.update.unwrap_or_default();
        let mut populated = Vec::new();
        if upd.enabled.is_some() {
            populated.push("enabled");
        }
        if upd.limit_type.is_some() {
            populated.push("limit_type");
        }
        if upd.limit_value.is_some() {
            populated.push("limit_value");
        }
        if upd.burst_value.is_some() {
            populated.push("burst_value");
        }
        if upd.connection_id.is_some() {
            populated.push("connection_id");
        }
        if !upd.models.is_empty() {
            populated.push("models");
        }
        let resolved = field_mask::resolve(&body.update_mask, &populated)?;

        let input = if resolved.is_noop() {
            UpdateRateLimitInput::default()
        } else {
            let has_burst_value = upd.burst_value.is_some();
            let has_connection_id = upd.connection_id.is_some();
            let mut target = RateLimitPatch::default();
            let source = RateLimitPatch {
                enabled: upd.enabled.unwrap_or_default(),
                limit_type: upd.limit_type.unwrap_or_default(),
                limit_value: upd.limit_value.unwrap_or_default(),
                burst_value: upd.burst_value,
                connection_id: upd.connection_id,
                models: upd.models,
            };
            field_mask::apply(&resolved, &mut target, source)?;
            UpdateRateLimitInput {
                enabled: resolved.contains("enabled").then_some(target.enabled),
                limit_type: resolved.contains("limit_type").then_some(target.limit_type),
                limit_value: resolved
                    .contains("limit_value")
                    .then_some(target.limit_value),
                burst_value: field_mask::clearable_update(
                    &resolved,
                    "burst_value",
                    has_burst_value,
                    target.burst_value,
                ),
                connection_id: field_mask::clearable_update(
                    &resolved,
                    "connection_id",
                    has_connection_id,
                    target.connection_id,
                ),
                models: resolved.contains("models").then_some(target.models),
            }
        };

        let policy = state.rate_limit_service().update(&body.id, input).await?;
        Ok(Response::new(proto::UpdateRateLimitResponse {
            policy: Some(conv::rate_limit_to_proto(&policy)),
        }))
    }

    async fn delete_rate_limit(
        &self,
        req: Request<proto::DeleteRateLimitRequest>,
    ) -> Result<Response<proto::DeleteRateLimitResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        state.rate_limit_service().delete(&body.id).await?;
        Ok(Response::new(proto::DeleteRateLimitResponse {
            id: body.id,
        }))
    }
}

#[cfg(test)]
mod tests {
    use prost_types::FieldMask;

    use super::{field_mask, RateLimitPatch};

    #[test]
    fn update_mask_rejects_ownership_fields() {
        for field in ["scope", "scope_id"] {
            let resolved = field_mask::resolve(
                &Some(FieldMask {
                    paths: vec![field.to_string()],
                }),
                &[],
            )
            .unwrap();
            let error = field_mask::apply(
                &resolved,
                &mut RateLimitPatch::default(),
                RateLimitPatch::default(),
            )
            .expect_err("rate-limit ownership must not be mutable");
            assert_eq!(error.code(), tonic::Code::InvalidArgument);
        }
    }
}
