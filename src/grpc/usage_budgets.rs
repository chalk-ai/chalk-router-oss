use super::proto::usage_budget_service_server::UsageBudgetService;
use super::{conv, field_mask, proto};
use crate::{budget::BudgetPolicy, AppState};
use chrono::Utc;
use tonic::{Request, Response, Status};

pub struct UsageBudgetServer {
    pub state: AppState,
}
#[allow(deprecated)] // Legacy daily aliases remain part of the public wire contract.
fn to_proto(
    p: &BudgetPolicy,
    current_usage: Option<u64>,
    next_reset_at: chrono::DateTime<Utc>,
) -> proto::UsageBudget {
    proto::UsageBudget {
        id: p.id.clone(),
        scope: p.scope.clone(),
        scope_id: p.scope_id.clone(),
        daily_token_budget: if p.period == "daily" {
            p.limit_amount
        } else {
            0
        },
        token_limit: p.limit_amount,
        period: match p.period.as_str() {
            "weekly" => proto::UsageBudgetPeriod::Weekly as i32,
            "monthly" => proto::UsageBudgetPeriod::Monthly as i32,
            _ => proto::UsageBudgetPeriod::Daily as i32,
        },
        enabled: p.enabled,
        created_at: Some(conv::ts_to_proto(p.created_at)),
        current_usage,
        next_reset_at: Some(conv::ts_to_proto(next_reset_at)),
    }
}

/// Thin adapter over [`crate::service::UsageBudgetService::with_usage`]: the
/// quota-backend lookup itself lives in the service layer now, this just
/// translates the enriched domain result into wire types.
#[allow(clippy::result_large_err)] // tonic service methods use Status directly.
async fn with_runtime(
    state: &AppState,
    policies: Vec<BudgetPolicy>,
) -> Result<Vec<proto::UsageBudget>, Status> {
    let enriched = state.usage_budget_service().with_usage(policies).await?;
    Ok(enriched
        .into_iter()
        .map(|e| to_proto(&e.policy, e.current_usage, e.window_end))
        .collect())
}

#[tonic::async_trait]
#[allow(deprecated)] // Interpret the public legacy daily aliases.
impl UsageBudgetService for UsageBudgetServer {
    async fn create_usage_budget(
        &self,
        req: Request<proto::CreateUsageBudgetRequest>,
    ) -> Result<Response<proto::CreateUsageBudgetResponse>, Status> {
        let state = self.state.for_request(&req);
        let b = req.into_inner();
        if b.daily_token_budget != 0 && b.token_limit != 0 {
            return Err(Status::invalid_argument(
                "daily_token_budget cannot be combined with token_limit",
            ));
        }
        let (token_limit, period) = if b.daily_token_budget != 0 {
            (b.daily_token_budget, "daily".to_string())
        } else {
            if b.token_limit == 0 {
                return Err(Status::invalid_argument("token_limit must be positive"));
            }
            let period = match proto::UsageBudgetPeriod::try_from(b.period)
                .unwrap_or(proto::UsageBudgetPeriod::Unspecified)
            {
                proto::UsageBudgetPeriod::Daily => "daily",
                proto::UsageBudgetPeriod::Weekly => "weekly",
                proto::UsageBudgetPeriod::Monthly => "monthly",
                proto::UsageBudgetPeriod::Unspecified => {
                    return Err(Status::invalid_argument(
                        "period must be DAILY, WEEKLY, or MONTHLY",
                    ));
                }
            };
            (b.token_limit, period.to_string())
        };
        let p = state
            .usage_budget_service()
            .create(
                b.scope,
                b.scope_id,
                token_limit,
                period,
                b.enabled.unwrap_or(true),
            )
            .await?;
        let budget = with_runtime(&state, vec![p]).await?.pop();
        Ok(Response::new(proto::CreateUsageBudgetResponse { budget }))
    }
    async fn list_usage_budgets(
        &self,
        req: Request<proto::ListUsageBudgetsRequest>,
    ) -> Result<Response<proto::ListUsageBudgetsResponse>, Status> {
        let state = self.state.for_request(&req);
        let request = req.into_inner();
        let policies = state
            .usage_budget_service()
            .list(request.scope, request.scope_id)
            .await?;
        Ok(Response::new(proto::ListUsageBudgetsResponse {
            budgets: with_runtime(&state, policies).await?,
        }))
    }
    async fn update_usage_budget(
        &self,
        req: Request<proto::UpdateUsageBudgetRequest>,
    ) -> Result<Response<proto::UpdateUsageBudgetResponse>, Status> {
        let state = self.state.for_request(&req);
        let b = req.into_inner();
        let u = b.update.unwrap_or_default();
        let populated = [
            u.daily_token_budget
                .is_some()
                .then_some("daily_token_budget"),
            u.token_limit.is_some().then_some("token_limit"),
            u.enabled.is_some().then_some("enabled"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        let r = field_mask::resolve(&b.update_mask, &populated)?;
        if let field_mask::ResolvedPaths::Paths(paths) = &r {
            for p in paths {
                if p == "period" {
                    return Err(Status::invalid_argument(
                        "update_mask: period is immutable; delete and recreate the usage budget",
                    ));
                }
                if p != "daily_token_budget" && p != "token_limit" && p != "enabled" {
                    return Err(Status::invalid_argument(
                        "update_mask: unsupported usage-budget field",
                    ));
                }
            }
        }
        if r.contains("daily_token_budget") && r.contains("token_limit") {
            return Err(Status::invalid_argument(
                "update_mask cannot include both daily_token_budget and token_limit",
            ));
        }
        let amount = r
            .contains("daily_token_budget")
            .then_some(u.daily_token_budget.unwrap_or_default())
            .or_else(|| {
                r.contains("token_limit")
                    .then_some(u.token_limit.unwrap_or_default())
            });
        if amount == Some(0) {
            return Err(Status::invalid_argument("token_limit must be positive"));
        }
        if r.contains("daily_token_budget") {
            let existing = state.usage_budget_service().get(&b.id).await?;
            if existing.period != "daily" {
                return Err(Status::invalid_argument(
                    "daily_token_budget is only valid for DAILY budgets",
                ));
            }
        }
        let p = state
            .usage_budget_service()
            .update(
                &b.id,
                amount,
                r.contains("enabled")
                    .then_some(u.enabled.unwrap_or_default()),
            )
            .await?;
        let budget = with_runtime(&state, vec![p]).await?.pop();
        Ok(Response::new(proto::UpdateUsageBudgetResponse { budget }))
    }
    async fn get_usage_budget(
        &self,
        req: Request<proto::GetUsageBudgetRequest>,
    ) -> Result<Response<proto::GetUsageBudgetResponse>, Status> {
        let state = self.state.for_request(&req);
        let id = req.into_inner().id;
        let p = state.usage_budget_service().get(&id).await?;
        let budget = with_runtime(&state, vec![p]).await?.pop();
        Ok(Response::new(proto::GetUsageBudgetResponse { budget }))
    }
    async fn delete_usage_budget(
        &self,
        req: Request<proto::DeleteUsageBudgetRequest>,
    ) -> Result<Response<proto::DeleteUsageBudgetResponse>, Status> {
        let state = self.state.for_request(&req);
        let id = req.into_inner().id;
        state.usage_budget_service().delete(&id).await?;
        Ok(Response::new(proto::DeleteUsageBudgetResponse { id }))
    }
}
