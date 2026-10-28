use tonic::{Request, Response, Status};

use crate::service::usage_pools::{CreateUsagePoolInput, UpdateUsagePoolInput, UsagePoolLookup};
use crate::AppState;

use super::conv;
use super::field_mask;
use super::proto;
use super::proto::usage_pool_service_server::UsagePoolService;

pub struct UsagePoolServer {
    pub state: AppState,
}

#[tonic::async_trait]
impl UsagePoolService for UsagePoolServer {
    async fn create_usage_pool(
        &self,
        req: Request<proto::CreateUsagePoolRequest>,
    ) -> Result<Response<proto::CreateUsagePoolResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        let usage_budgets = body
            .usage_budgets
            .into_iter()
            .map(conv::api_key_budget_from_proto)
            .collect::<Result<Vec<_>, _>>()?;
        let pool = state
            .usage_pool_service()
            .create(CreateUsagePoolInput {
                name: body.name,
                rate_limits: body
                    .rate_limits
                    .into_iter()
                    .map(conv::api_key_rate_limit_from_proto)
                    .collect(),
                usage_budgets,
            })
            .await?;
        Ok(Response::new(proto::CreateUsagePoolResponse {
            pool: Some(conv::usage_pool_to_proto(&pool)),
        }))
    }

    async fn update_usage_pool(
        &self,
        req: Request<proto::UpdateUsagePoolRequest>,
    ) -> Result<Response<proto::UpdateUsagePoolResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        let update = body.update.unwrap_or_default();
        let mut populated = Vec::new();
        if update.name.is_some() {
            populated.push("name");
        }
        if !update.rate_limits.is_empty() {
            populated.push("rate_limits");
        }
        if !update.usage_budgets.is_empty() {
            populated.push("usage_budgets");
        }
        let resolved = field_mask::resolve(&body.update_mask, &populated)?;
        if let field_mask::ResolvedPaths::Paths(paths) = &resolved {
            for path in paths {
                if !matches!(path.as_str(), "name" | "rate_limits" | "usage_budgets") {
                    return Err(Status::invalid_argument(format!(
                        "update_mask: unsupported usage-pool field '{path}'"
                    )));
                }
            }
        }

        let usage_budgets = if resolved.contains("usage_budgets") {
            Some(
                update
                    .usage_budgets
                    .into_iter()
                    .map(conv::api_key_budget_from_proto)
                    .collect::<Result<Vec<_>, _>>()?,
            )
        } else {
            None
        };
        let rate_limits = resolved.contains("rate_limits").then(|| {
            update
                .rate_limits
                .into_iter()
                .map(conv::api_key_rate_limit_from_proto)
                .collect()
        });
        let pool = state
            .usage_pool_service()
            .update(
                &body.id,
                UpdateUsagePoolInput {
                    name: resolved
                        .contains("name")
                        .then_some(update.name.unwrap_or_default()),
                    rate_limits,
                    usage_budgets,
                },
            )
            .await?;
        Ok(Response::new(proto::UpdateUsagePoolResponse {
            pool: Some(conv::usage_pool_to_proto(&pool)),
        }))
    }

    async fn list_usage_pools(
        &self,
        req: Request<proto::ListUsagePoolsRequest>,
    ) -> Result<Response<proto::ListUsagePoolsResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        let (pools, next_cursor) = state
            .usage_pool_service()
            .list_page(body.limit, body.cursor)
            .await?;
        Ok(Response::new(proto::ListUsagePoolsResponse {
            pools: pools.iter().map(conv::usage_pool_to_proto).collect(),
            next_cursor,
        }))
    }

    async fn get_usage_pool(
        &self,
        req: Request<proto::GetUsagePoolRequest>,
    ) -> Result<Response<proto::GetUsagePoolResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        let lookup = body.lookup.map(|lookup| match lookup {
            proto::get_usage_pool_request::Lookup::Id(id) => UsagePoolLookup::Id(id),
            proto::get_usage_pool_request::Lookup::Name(name) => UsagePoolLookup::Name(name),
        });
        let pool = state.usage_pool_service().lookup(lookup).await?;
        Ok(Response::new(proto::GetUsagePoolResponse {
            pool: Some(conv::usage_pool_to_proto(&pool)),
        }))
    }

    async fn delete_usage_pool(
        &self,
        req: Request<proto::DeleteUsagePoolRequest>,
    ) -> Result<Response<proto::DeleteUsagePoolResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        state.usage_pool_service().delete(&body.id).await?;
        Ok(Response::new(proto::DeleteUsagePoolResponse {
            id: body.id,
        }))
    }
}
