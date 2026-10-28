use crate::tenancy::RequestState as State;
use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::backend::provider::forwardable_upstream_headers;
use crate::backend::{BackendError, ResolvedRoute};
use crate::capabilities::{
    CapabilityRequirements, InputModality, ModelFeature, ModelSurface, OutputModality, Protocol,
    ProtocolOperation, Support,
};
use crate::decisions::{
    DecisionsIngress, DecisionsRequest, DecisionsResponse, SystemOneRequest, SystemOneResponse,
};
use crate::middleware::ResolvedApiKey;
use crate::translation::decisions_openai::{self, OpenAiAnswerPlan, OpenAiDecisionsRequest};
use crate::usage_estimator::NormalizedUsageInput;

use super::routing::{
    handle_raw_upstream_dispatch, raw_upstream_failure_response, raw_upstream_response,
    set_route_span_attributes, stamp_routing_headers, try_with_fallbacks_if, RawUpstreamFailure,
    RoutedRequest,
};
use super::trace_attributes::{ContentDirection, LlmSpanKind, LlmSpanWriter};
use super::usage::{self, TokenUsage, UsageAttribution};

impl RoutedRequest for DecisionsRequest {
    fn set_model(&mut self, model: String) {
        self.model = model;
    }

    fn check_capability(&self, route: &ResolvedRoute) -> Result<(), String> {
        let requirements = decisions_requirements(self);
        crate::capabilities::trace_decision(
            route,
            &self.model,
            &requirements,
            match self.ingress {
                DecisionsIngress::Alpha => "router_decisions.create",
                DecisionsIngress::SystemOne => "typesafe_system_one.create",
                DecisionsIngress::OpenAi => "openai_decisions.create",
            },
        );
        route
            .capabilities
            .profile
            .satisfies(&requirements)
            .then_some(())
            .ok_or_else(|| super::routing::NO_LOSSLESS_ROUTE.to_string())
    }

    fn capability_support(&self, route: &ResolvedRoute) -> Support {
        route
            .capabilities
            .profile
            .support_for(&decisions_requirements(self))
    }

    fn conservative_input_tokens(&self) -> Option<u64> {
        serde_json::to_value(self)
            .ok()
            .as_ref()
            .and_then(NormalizedUsageInput::from_json)
            .map(|input| input.conservative_tokens())
    }

    fn rate_limit_tokens(
        &self,
        route: &ResolvedRoute,
        input_tokens: Option<u64>,
    ) -> (Option<u64>, Option<u64>) {
        let reservation = input_tokens
            .zip(route.capabilities.profile.limits.max_output_tokens)
            .map(|(input, output)| input.saturating_add(output));
        (reservation, input_tokens)
    }
}

/// OpenAI-format requests are translated to the canonical Router Decisions
/// request before routing, so they need what a Router Decisions request needs.
fn decisions_requirements(request: &DecisionsRequest) -> CapabilityRequirements {
    let protocol = match request.ingress {
        DecisionsIngress::Alpha | DecisionsIngress::OpenAi => Protocol::RouterDecisions,
        DecisionsIngress::SystemOne => Protocol::TypesafeSystemOne,
    };
    let mut requirements =
        CapabilityRequirements::for_operation(protocol, ProtocolOperation::Create)
            .surface(ModelSurface::Decisions)
            .input(InputModality::Text)
            .output(OutputModality::Decisions);
    if !request.assets.is_empty() {
        requirements = requirements.input(InputModality::Image);
    }
    if request.has_box_questions() {
        requirements = requirements.feature(ModelFeature::DecisionBoxes);
    }
    requirements
}

async fn dispatch_decision(
    backend: std::sync::Arc<dyn crate::backend::BackendProvider>,
    request: DecisionsRequest,
    raw_failure: &std::sync::Mutex<Option<RawUpstreamFailure>>,
) -> Result<reqwest::Response, BackendError> {
    let result = backend.send_decisions_request(request).await;
    handle_raw_upstream_dispatch(result, raw_failure).await
}

pub async fn create_decision(
    State(state): State<crate::AppState>,
    api_key: ResolvedApiKey,
    headers: HeaderMap,
    Json(mut request): Json<DecisionsRequest>,
) -> Response {
    request.ingress = DecisionsIngress::Alpha;
    create(&state, &api_key, &headers, request, None).await
}

/// `POST /v1/decisions`: OpenAI's Decisions API, served by any decision model.
pub async fn create_openai_decision(
    State(state): State<crate::AppState>,
    api_key: ResolvedApiKey,
    headers: HeaderMap,
    Json(request): Json<OpenAiDecisionsRequest>,
) -> Response {
    match decisions_openai::ingress_to_canonical(request) {
        Ok((request, plan)) => create(&state, &api_key, &headers, request, Some(plan)).await,
        Err(error) => super::invalid_request_response(error),
    }
}

pub async fn create_system_one(
    State(state): State<crate::AppState>,
    api_key: ResolvedApiKey,
    headers: HeaderMap,
    Json(request): Json<SystemOneRequest>,
) -> Response {
    create(
        &state,
        &api_key,
        &headers,
        DecisionsRequest::from_system_one(request),
        None,
    )
    .await
}

async fn create(
    state: &crate::AppState,
    api_key: &ResolvedApiKey,
    headers: &HeaderMap,
    request: DecisionsRequest,
    plan: Option<OpenAiAnswerPlan>,
) -> Response {
    let span = match request.ingress {
        DecisionsIngress::OpenAi => {
            tracing::info_span!("openai_decisions", otel.name = "openai.decisions")
        }
        DecisionsIngress::Alpha => {
            tracing::info_span!("router_decisions", otel.name = "router.decisions")
        }
        DecisionsIngress::SystemOne => {
            tracing::info_span!("typesafe_system_one", otel.name = "typesafe.systemone")
        }
    };
    let span = LlmSpanWriter::new(span, state.object_store.clone(), LlmSpanKind::Llm);
    super::stamp_authenticated_principal(&span, api_key);

    span.clone()
        .instrument(async move {
            if let Err(error) = request.validate() {
                return super::invalid_request_response(error);
            }
            let session_id = match request.session_id.as_deref() {
                Some(session_id) => Some(session_id),
                None => match headers.get("x-session-id") {
                    Some(value) => match value.to_str() {
                        Ok(value) => {
                            if let Err(error) = crate::decisions::validate_session_id(value) {
                                return super::invalid_request_response(error);
                            }
                            Some(value)
                        }
                        Err(_) => {
                            return super::invalid_request_response(
                                "`x-session-id` must be valid UTF-8".to_string(),
                            );
                        }
                    },
                    None => None,
                },
            };
            span.set_session_id(session_id);
            span.record_json(ContentDirection::Input, &request);
            proxy_decision(state, api_key, request, plan, &span).await
        })
        .await
}

async fn proxy_decision(
    state: &crate::AppState,
    api_key: &ResolvedApiKey,
    request: DecisionsRequest,
    plan: Option<OpenAiAnswerPlan>,
    span: &LlmSpanWriter,
) -> Response {
    let ingress = request.ingress;
    let original_model = request.model.clone();
    let route = match state.backend.resolve(&original_model).await {
        Ok(route) => route,
        Err(error) => return super::backend_error_response(&error),
    };
    set_route_span_attributes(span, &route.resolution);

    if api_key.0.permission.is_restricted() {
        if let Err(denied) = api_key
            .0
            .permission
            .allows_route(&route.resolution, &original_model)
        {
            return super::forbidden_response(denied.to_string());
        }
    }

    let unsupported_message = match ingress {
        DecisionsIngress::Alpha => "no configured provider supports Router Decisions requests",
        DecisionsIngress::OpenAi => "no configured provider supports Decisions requests",
        DecisionsIngress::SystemOne => {
            "no configured provider supports TypeSafe System One requests"
        }
    };
    let raw_failure = std::sync::Mutex::new(None::<RawUpstreamFailure>);
    let input_tokens = request.conservative_input_tokens();
    let result = try_with_fallbacks_if(
        span,
        state,
        request,
        super::routing::RoutePlan::configured(original_model.clone(), route),
        api_key,
        input_tokens,
        |route, request| {
            request
                .capability_eligible(route)
                .then_some(())
                .ok_or_else(|| unsupported_message.to_string())
        },
        |route, request| {
            let backend = route.backend.clone();
            dispatch_decision(backend, request, &raw_failure)
        },
    )
    .await;

    let (upstream, fallback_model, mut resolution, mut permit) = match result {
        Ok(result) => result,
        Err(error) => {
            return match raw_failure.into_inner().ok().flatten() {
                Some(raw) => raw_upstream_failure_response(raw),
                None => super::backend_error_response(&error),
            };
        }
    };
    let served_model = fallback_model.as_deref().unwrap_or(&original_model);
    set_route_span_attributes(span, &resolution);
    span.set_requested_served_models(&original_model, served_model);

    if !upstream.status().is_success() {
        if let Some(permit) = permit.take() {
            permit.finish_failed().await;
        }
        return raw_upstream_response(upstream).await;
    }

    let headers = forwardable_upstream_headers(upstream.headers());
    let mut response = match upstream.json::<DecisionsResponse>().await {
        Ok(response) => response,
        Err(error) => {
            if let Some(permit) = permit.take() {
                permit.finish(None).await;
            }
            return super::backend_error_response(&BackendError::api_status(
                502,
                format!("invalid Decisions response from upstream: {error}"),
            ));
        }
    };

    let reported_model = response.model.clone();
    resolution.upstream_model_id = match resolution.provider_kind.as_str() {
        "typesafe" => reported_model
            .strip_prefix("typesafe/")
            .unwrap_or(&reported_model),
        "openrouter" => reported_model
            .strip_prefix("openrouter/")
            .unwrap_or(&reported_model),
        _ => &reported_model,
    }
    .to_string();
    set_route_span_attributes(span, &resolution);

    if response.provider.is_none() {
        response.provider = crate::provider_registry::get(&resolution.provider_kind)
            .map(|provider| provider.display_name.to_string());
    }
    if let Some(prefix) = crate::provider_registry::canonical_prefix(&resolution.provider_kind) {
        if !response.model.starts_with(prefix) {
            response.model = format!("{prefix}{}", response.model);
        }
    }

    let token_usage = TokenUsage::from_decisions(&response.usage);
    if response.usage.cost.is_none() {
        response.usage.cost = crate::pricing::cost_usd(
            &resolution.provider_kind,
            &resolution.upstream_model_id,
            &crate::pricing::TokenUsage {
                prompt: token_usage.input_tokens,
                completion: token_usage.output_tokens,
                ..Default::default()
            },
        )
        .map(|cost| cost.total_usd);
    }
    token_usage.stamp(span);
    let write = usage::record_future(
        state.clone(),
        UsageAttribution::new(api_key, &resolution),
        token_usage,
    );
    if usage::with_quota_settlement(write, permit, Some(token_usage.total_tokens))
        .await
        .is_err()
    {
        return super::usage_write_error_response();
    }

    let (output, mut downstream) = match ingress {
        DecisionsIngress::Alpha => {
            let output = serde_json::to_value(&response).unwrap_or_default();
            (output, Json(response).into_response())
        }
        DecisionsIngress::SystemOne => {
            let response = SystemOneResponse::from(response);
            let output = serde_json::to_value(&response).unwrap_or_default();
            (output, Json(response).into_response())
        }
        DecisionsIngress::OpenAi => {
            let rendered = plan
                .as_ref()
                .ok_or_else(|| "OpenAI Decisions request lost its answer plan".to_string())
                .and_then(|plan| decisions_openai::canonical_to_ingress_response(plan, &response));
            match rendered {
                Ok(output) => (output.clone(), Json(output).into_response()),
                Err(error) => {
                    return super::backend_error_response(&BackendError::api_status(
                        502,
                        format!("invalid Decisions response from upstream: {error}"),
                    ));
                }
            }
        }
    };
    span.record_json(ContentDirection::Output, &output);
    span.apply_outcome_sampling(&state.sampling_gate, &output);

    super::apply_forwarded_headers(downstream.headers_mut(), &headers);
    super::stamp_trace_headers(downstream.headers_mut());
    downstream.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    stamp_routing_headers(
        downstream.headers_mut(),
        &original_model,
        served_model,
        fallback_model.as_deref(),
    );
    downstream
}
