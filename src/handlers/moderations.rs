use crate::capability_requirements::moderation_requirements;
use crate::middleware::ResolvedApiKey;
use crate::quota::TokenReservation;
use crate::ratelimit::RateLimitModel;
use crate::tenancy::RequestState as State;
use crate::types::CreateModerationRequest;
use axum::{
    extract::Json,
    response::{IntoResponse, Response},
};

use super::routing::{acquire_single_route_quota, run_passthrough};
use super::trace_attributes::{ContentDirection, LlmSpanKind, LlmSpanWriter};
use super::usage::{record_future, with_quota_settlement, TokenUsage, UsageAttribution};

/// Text moderation is a single-route, request-metered operation. Providers do
/// not report token usage, so token policies do not apply and successful calls
/// enter the usage ledger with zero tokens.
pub async fn create_moderation(
    State(state): State<crate::AppState>,
    api_key: ResolvedApiKey,
    Json(mut req): Json<CreateModerationRequest>,
) -> Response {
    let span = tracing::info_span!("create_moderation", otel.name = "moderation.create");
    let span = LlmSpanWriter::new(span, state.object_store.clone(), LlmSpanKind::Llm);
    super::stamp_authenticated_principal(&span, &api_key);

    span.clone()
        .instrument(async move {
            let requested_model = req
                .model
                .clone()
                .unwrap_or_else(|| "omni-moderation-latest".to_string());
            span.record_json(ContentDirection::Input, &req.input);
            let route = match run_passthrough(
                &state,
                &api_key,
                &span,
                &requested_model,
                &moderation_requirements(),
                "openai_moderations.create",
            )
            .await
            {
                Ok(route) => route,
                Err(error) => return super::backend_error_response(&error),
            };
            let permit = match acquire_single_route_quota(
                &state,
                &api_key,
                &route.resolution,
                RateLimitModel::Exact(&requested_model),
                TokenReservation::NotApplicable,
                None,
            )
            .await
            {
                Ok(permit) => permit,
                Err(error) => return super::backend_error_response(&error),
            };

            req.model = Some(route.stripped_model.clone());
            match route.backend.create_moderation(req).await {
                Ok(response) => {
                    span.record_json(ContentDirection::Output, &response);
                    if with_quota_settlement(
                        record_future(
                            state.clone(),
                            UsageAttribution::new(&api_key, &route.resolution),
                            TokenUsage::default(),
                        ),
                        permit,
                        None,
                    )
                    .await
                    .is_err()
                    {
                        return super::usage_write_error_response();
                    }
                    let mut response = Json(response).into_response();
                    super::stamp_trace_headers(response.headers_mut());
                    response
                }
                Err(error) => {
                    if let Some(permit) = permit {
                        permit.finish_failed().await;
                    }
                    span.set_error("backend_error", Some(&error.to_string()));
                    super::backend_error_response(&error)
                }
            }
        })
        .await
}
