//! Protocol-neutral routing and fallback dispatch for LLM generation endpoints.
//!
//! Chat Completions and Responses use different wire shapes, but they must make
//! exactly the same routing decision: resolve once, authorize that resolution,
//! dispatch directly to the resolved leaf backend, and only retry fallbacks that
//! were themselves strict-resolved and authorized.

use std::ops::Deref;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use crate::backend::anthropic::ANTHROPIC_DEFAULT_MAX_TOKENS;
use crate::backend::provider::forwardable_upstream_headers;
use crate::backend::{BackendError, BackendProvider, ResolvedRoute, RouteResolution};
use crate::capabilities::{
    CapabilityRequirements, DeliveryMode, EndpointFeature, InputModality, ModelFeature,
    ModelSurface, OutputModality, Protocol, ProtocolOperation, Support,
};
use crate::handlers::trace_attributes::LlmSpanWriter;
use crate::middleware::ResolvedApiKey;
use crate::provider_registry::ROUTE_RESERVED_PREFIX;
use crate::quota::{
    QuotaAction, QuotaDecision, QuotaExceeded, QuotaPermit, QuotaSource, TokenReservation,
};
use crate::ratelimit::{RateLimitMatch, RateLimitModel};
use crate::usage_estimator::{NormalizedUsageInput, ToolContext};
use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;

/// Default per-attempt stream-open deadline: well above a healthy headers
/// response from every provider we route to, well below any client's patience
/// for a hung stream.
const DEFAULT_STREAM_OPEN_TIMEOUT_MS: u64 = 10_000;

/// Compatibility Responses streams are synthesized only after a complete
/// non-streaming Chat Completions request. They cannot use the short
/// headers-only deadline, but still need a finite ceiling so a stalled
/// provider can yield to a configured fallback.
const DEFAULT_RESPONSES_COMPAT_TIMEOUT_MS: u64 = 300_000;

pub(super) const NO_LOSSLESS_ROUTE: &str =
    "no configured provider can represent this request losslessly";
const UNKNOWN_TOKEN_ESTIMATE: &str = "token quota cannot admit a request without a conservative token estimate; if using stored conversation or cached-history references, supply the full history inline instead; tool types the router does not recognize cannot be estimated";

/// How long one candidate gets to *open* its stream before we give up on it and
/// move to the next (RFP-0002 §5.4).
///
/// This bounds only the upstream response headers. Once a stream opens, its body
/// is unbounded so healthy long generations are unaffected. Non-streaming calls
/// deliberately do not use this deadline because their future completes only
/// after the full generation has been produced.
static STREAM_OPEN_TIMEOUT: LazyLock<Duration> = LazyLock::new(|| {
    let ms = std::env::var("ROUTER_STREAM_OPEN_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(DEFAULT_STREAM_OPEN_TIMEOUT_MS);
    Duration::from_millis(ms)
});

static RESPONSES_COMPAT_TIMEOUT: LazyLock<Duration> = LazyLock::new(|| {
    let ms = std::env::var("ROUTER_RESPONSES_COMPAT_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(DEFAULT_RESPONSES_COMPAT_TIMEOUT_MS);
    Duration::from_millis(ms)
});

/// A request whose public model id can be replaced with the already-resolved
/// upstream id immediately before dispatch.
pub(crate) trait RoutedRequest: Clone {
    fn set_model(&mut self, model: String);

    fn check_capability(&self, route: &ResolvedRoute) -> Result<(), String>;

    fn capability_eligible(&self, route: &ResolvedRoute) -> bool {
        self.check_capability(route).is_ok()
    }

    fn capability_support(&self, route: &ResolvedRoute) -> Support;

    /// Conservative input-token estimate, derived by serializing the whole
    /// request to JSON and running it through [`NormalizedUsageInput`]. This is
    /// the same for every candidate route a request is considered against, so
    /// callers compute it once per request and pass it into
    /// [`RoutedRequest::rate_limit_tokens`] for each candidate/attempt instead
    /// of paying for the serialization again per route.
    fn conservative_input_tokens(&self) -> Option<u64>;

    fn rate_limit_tokens(
        &self,
        route: &ResolvedRoute,
        input_tokens: Option<u64>,
    ) -> (Option<u64>, Option<u64>);

    /// Whether the provider reads input beyond the visible estimate (hosted
    /// tool output, stored history, opaque content such as images or
    /// encrypted reasoning), making every token reservation for this request
    /// a soft cap. Must be true whenever the admission input passed to
    /// [`try_with_fallbacks_if`] leaves anything out.
    fn soft_cap(&self) -> bool {
        false
    }

    fn background_accounting_id(&self) -> Option<uuid::Uuid> {
        None
    }
}

/// Check whether the resolved adapter can preserve the native Chat request.
pub(crate) fn route_check_native_chat(
    route: &ResolvedRoute,
    native: &crate::native_chat::NativeChatRequest,
) -> Result<(), String> {
    route
        .backend
        .check_native_chat(native, &route.resolution.upstream_model_id)
        .map_err(|error| super::backend_error_body(&error).error.message)
}

impl RoutedRequest for crate::types::ChatCompletionRequest {
    fn set_model(&mut self, model: String) {
        self.model = model;
    }

    fn check_capability(&self, route: &ResolvedRoute) -> Result<(), String> {
        let requirements = chat_requirements(self);
        crate::capabilities::trace_decision(
            route,
            &self.model,
            &requirements,
            "openai_chat_completions.create",
        );
        route
            .capabilities
            .profile
            .satisfies(&requirements)
            .then_some(())
            .ok_or_else(|| NO_LOSSLESS_ROUTE.to_string())
    }

    fn capability_support(&self, route: &ResolvedRoute) -> Support {
        route
            .capabilities
            .profile
            .support_for(&chat_requirements(self))
    }

    fn conservative_input_tokens(&self) -> Option<u64> {
        serde_json::to_value(self)
            .ok()
            .as_ref()
            .and_then(NormalizedUsageInput::visible_json)
            .map(|input| input.conservative_tokens())
    }

    fn soft_cap(&self) -> bool {
        // The same walk `visible_json` strips, so the flag covers exactly what
        // the admission estimate leaves out.
        serde_json::to_value(self)
            .ok()
            .is_some_and(|value| crate::usage_estimator::provider_adds_input(&value))
    }

    fn rate_limit_tokens(
        &self,
        route: &ResolvedRoute,
        input_tokens: Option<u64>,
    ) -> (Option<u64>, Option<u64>) {
        let requested_output = self
            .max_completion_tokens
            .or(self.max_tokens)
            .and_then(|value| u64::try_from(value).ok());
        let output_tokens =
            requested_output.or(route.capabilities.profile.limits.max_output_tokens);
        let choices = self
            .n
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(1);
        let reservation = input_tokens
            .zip(output_tokens)
            .map(|(input, output)| input.saturating_add(output.saturating_mul(choices)));
        (reservation, input_tokens)
    }
}

impl RoutedRequest for crate::native_chat::NativeChatRequest {
    fn set_model(&mut self, model: String) {
        self.set_model(model);
    }
    fn check_capability(&self, route: &ResolvedRoute) -> Result<(), String> {
        route_check_native_chat(route, self)?;
        self.deref().check_capability(route)
    }
    fn capability_support(&self, route: &ResolvedRoute) -> Support {
        self.deref().capability_support(route)
    }
    fn conservative_input_tokens(&self) -> Option<u64> {
        self.normalized_usage_input()
            .or_else(|| self.soft_cap_admission_input())
            .map(|input| input.conservative_tokens())
    }
    fn soft_cap(&self) -> bool {
        // Native fields the typed view drops still count toward the estimate.
        crate::usage_estimator::provider_adds_input(&self.value())
    }
    fn rate_limit_tokens(
        &self,
        route: &ResolvedRoute,
        input_tokens: Option<u64>,
    ) -> (Option<u64>, Option<u64>) {
        self.deref().rate_limit_tokens(route, input_tokens)
    }
}

impl RoutedRequest for crate::responses::ResponsesRequest {
    fn set_model(&mut self, model: String) {
        self.set_model(model);
    }

    fn check_capability(&self, _route: &ResolvedRoute) -> Result<(), String> {
        // Responses supplies its richer request-shape predicate through
        // `try_with_fallbacks_if`.
        Ok(())
    }

    fn capability_support(&self, route: &ResolvedRoute) -> Support {
        route
            .capabilities
            .profile
            .support_for(&crate::handlers::responses::responses_requirements(self))
    }

    fn conservative_input_tokens(&self) -> Option<u64> {
        serde_json::to_value(self)
            .ok()
            .as_ref()
            .and_then(NormalizedUsageInput::visible_responses_json)
            .map(|input| input.conservative_tokens())
    }

    fn soft_cap(&self) -> bool {
        ToolContext::of_responses(self.raw()) == ToolContext::Hosted
            || crate::usage_estimator::provider_adds_input(self.raw())
    }

    fn rate_limit_tokens(
        &self,
        route: &ResolvedRoute,
        input_tokens: Option<u64>,
    ) -> (Option<u64>, Option<u64>) {
        // Normalization leaves unclassified tools unknown for foreground and
        // background requests alike; stored history is a soft cap.
        let requested_output = self
            .raw()
            .get("max_output_tokens")
            .and_then(serde_json::Value::as_u64);
        let uses_anthropic_chat_adapter = route.resolution.provider_kind == "anthropic"
            && route
                .capabilities
                .profile
                .operation(Protocol::OpenaiResponses, ProtocolOperation::Create)
                .is_some_and(|operation| {
                    matches!(
                        operation.mode,
                        DeliveryMode::Translated | DeliveryMode::Unknown
                    )
                });
        // Anthropic requires `max_tokens`, so its chat adapter supplies a
        // finite default when the Responses request and model facts omit one.
        let output_tokens = requested_output
            .or(route.capabilities.profile.limits.max_output_tokens)
            .or(uses_anthropic_chat_adapter.then_some(ANTHROPIC_DEFAULT_MAX_TOKENS));
        (
            input_tokens
                .zip(output_tokens)
                .map(|(input, output)| input.saturating_add(output)),
            input_tokens,
        )
    }

    fn background_accounting_id(&self) -> Option<uuid::Uuid> {
        self.background_accounting_id()
    }
}

fn chat_requirements(request: &crate::types::ChatCompletionRequest) -> CapabilityRequirements {
    let mut requirements = CapabilityRequirements::for_operation(
        Protocol::OpenaiChatCompletions,
        ProtocolOperation::Create,
    )
    .surface(ModelSurface::Chat)
    .output(OutputModality::Text);

    if request.messages.iter().any(chat_message_has_text) {
        requirements = requirements.input(InputModality::Text);
    }

    if request.stream == Some(true) {
        requirements = requirements.endpoint_feature(EndpointFeature::Streaming);
    }
    let uses_tools = request
        .tools
        .as_ref()
        .is_some_and(|tools| !tools.is_empty())
        || request.messages.iter().any(|message| {
            message.role == "tool"
                || message
                    .tool_calls
                    .as_ref()
                    .is_some_and(|calls| !calls.is_empty())
        });
    if uses_tools {
        requirements = requirements
            .feature(ModelFeature::ToolCalling)
            .endpoint_feature(EndpointFeature::ToolCalling);
    }
    if request.messages.iter().any(chat_message_has_image) {
        requirements = requirements
            .input(InputModality::Image)
            .endpoint_feature(EndpointFeature::ImageInput);
    }
    if request
        .response_format
        .as_ref()
        .and_then(|format| format.get("type"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|kind| kind != "text")
    {
        requirements = requirements.feature(ModelFeature::StructuredOutput);
    }
    requirements
}

fn chat_message_has_image(message: &crate::types::ChatMessage) -> bool {
    message
        .content
        .as_ref()
        .and_then(serde_json::Value::as_array)
        .is_some_and(|parts| {
            parts.iter().any(|part| {
                matches!(
                    part.get("type").and_then(serde_json::Value::as_str),
                    Some("image_url" | "input_image")
                )
            })
        })
}

fn chat_message_has_text(message: &crate::types::ChatMessage) -> bool {
    match message.content.as_ref() {
        Some(serde_json::Value::String(_)) => true,
        Some(serde_json::Value::Array(parts)) => parts.iter().any(|part| {
            part.get("type").and_then(serde_json::Value::as_str) == Some("text")
                || part.get("text").is_some()
        }),
        _ => false,
    }
}

/// Bound one candidate's stream-open future without imposing a deadline on the
/// body that follows it. Dropping the timed-out future cancels the in-flight
/// connection attempt.
pub(crate) async fn open_stream_with_deadline<T, F>(open: F) -> Result<T, BackendError>
where
    F: std::future::Future<Output = Result<T, BackendError>>,
{
    match tokio::time::timeout(*STREAM_OPEN_TIMEOUT, open).await {
        Ok(inner) => inner,
        Err(_elapsed) => Err(BackendError::api_status(
            504,
            format!(
                "upstream stream-open timed out after {}ms",
                STREAM_OPEN_TIMEOUT.as_millis()
            ),
        )),
    }
}

/// Bound a compatibility Responses generation by a deliberately generous
/// total deadline. Unlike [`open_stream_with_deadline`], this future includes
/// the full non-streaming generation that must complete before synthesized SSE
/// can start, so it uses a separate configuration and much larger default.
pub(crate) async fn compat_response_with_deadline<T, F>(generation: F) -> Result<T, BackendError>
where
    F: std::future::Future<Output = Result<T, BackendError>>,
{
    response_with_deadline(*RESPONSES_COMPAT_TIMEOUT, generation).await
}

async fn response_with_deadline<T, F>(deadline: Duration, generation: F) -> Result<T, BackendError>
where
    F: std::future::Future<Output = Result<T, BackendError>>,
{
    match tokio::time::timeout(deadline, generation).await {
        Ok(inner) => inner,
        Err(_elapsed) => Err(BackendError::api_status(
            504,
            format!(
                "upstream compatibility generation timed out after {}ms",
                deadline.as_millis()
            ),
        )),
    }
}

/// Stamp OpenInference and Chalk attributes for an already-resolved upstream.
pub(crate) fn set_route_span_attributes(span: &LlmSpanWriter, resolution: &RouteResolution) {
    span.set_route(resolution);
}

fn backend_error_type(error: &BackendError) -> &'static str {
    match error {
        BackendError::RequestError(_) => "request_error",
        BackendError::ApiError { .. } => "api_error",
        BackendError::ConfigError(_) => "config_error",
        BackendError::CredentialUnreadable(_) => "credential_unreadable",
        BackendError::Other(_) => "other",
        BackendError::InvalidResponse(_) => "invalid_response",
        BackendError::RoutingDeadlineExceeded => "routing_deadline_exceeded",
        BackendError::PermissionDenied(_) => "permission_denied",
    }
}

fn indicates_capability_mismatch(error: &BackendError) -> bool {
    let BackendError::ApiError {
        message, status, ..
    } = error
    else {
        return false;
    };
    matches!(status, 400 | 404 | 405 | 422) && {
        let message = message.to_ascii_lowercase();
        message.contains("unsupported") || message.contains("does not support")
    }
}

pub(crate) fn definite_background_rejection(error: &BackendError) -> bool {
    matches!(error,BackendError::ApiError{status,..} if (400..=599).contains(status))
}

pub(crate) async fn acquire_response_lifecycle_quota(
    state: &crate::AppState,
    api_key: &ResolvedApiKey,
    resolution: &RouteResolution,
) -> Result<Option<QuotaPermit>, BackendError> {
    if !state.quota_backend.enabled() {
        return Ok(None);
    }
    let mut actions = state
        .quota_resolver
        .request_actions(
            &api_key.0.key_hash,
            api_key.0.usage_pool_id.as_deref(),
            TokenReservation::NotApplicable,
            chrono::Utc::now(),
        )
        .await
        .map_err(|error| BackendError::Other(error.to_string()))?
        .actions;
    let route_actions = state
        .quota_resolver
        .rate_actions(
            &RateLimitMatch {
                key_hash: &api_key.0.key_hash,
                usage_pool_id: api_key.0.usage_pool_id.as_deref(),
                connection_id: resolution.connection_id.as_deref(),
                model: RateLimitModel::NotApplicable,
            },
            TokenReservation::NotApplicable,
        )
        .await
        .map_err(|error| BackendError::Other(error.to_string()))?;
    actions.extend(route_actions.actions);
    actions.retain(|action| action.meter == crate::quota::QuotaMeter::Requests);
    match state.quota_backend.acquire_many(&actions, None).await {
        Ok(QuotaDecision::Allowed(grant)) => {
            Ok(grant.map(|grant| QuotaPermit::new(state.quota_backend.clone(), grant)))
        }
        Ok(QuotaDecision::Limited(exceeded)) => Err(quota_exceeded_error(exceeded)),
        Err(_) => Err(BackendError::api_status(
            503,
            "lifecycle request quota is temporarily unavailable",
        )),
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_attempt<R, T, F, Fut>(
    state: &crate::AppState,
    api_key: &ResolvedApiKey,
    candidate: &Candidate,
    mut req: R,
    attempt_index: usize,
    input_tokens: Option<u64>,
    walk_deadline: tokio::time::Instant,
    first_admission: Option<Result<Option<QuotaPermit>, BackendError>>,
    dispatch: &F,
) -> Result<(T, Option<QuotaPermit>), AttemptFailure>
where
    R: RoutedRequest,
    F: Fn(&ResolvedRoute, R) -> Fut,
    Fut: std::future::Future<Output = Result<T, BackendError>>,
{
    let route = &candidate.route;
    let advertised_support = req.capability_support(route);
    let owner = req
        .background_accounting_id()
        .map(|event_id| crate::quota::BackgroundOwner {
            environment_id: state.environment_id.clone(),
            event_id,
        });
    let permit = match first_admission {
        Some(admission) => admission,
        None => {
            let (token_reservation, failed_attempt_tokens) =
                req.rate_limit_tokens(route, input_tokens);
            acquire_rate_limit_owned(
                state,
                api_key,
                &route.resolution,
                RateLimitModel::Exact(&candidate.identity),
                TokenReservation::from_option(token_reservation).with_soft_cap(req.soft_cap()),
                failed_attempt_tokens,
                owner.as_ref(),
            )
            .await
        }
    }
    .map_err(|error| AttemptFailure {
        error,
        dispatched: false,
    })?;
    req.set_model(route.stripped_model.clone());
    let started_at = Instant::now();
    let result = if owner.is_some() {
        // A background submission returns once the job is queued. Bound a hung
        // submission; its quota stays with the durable background owner, which
        // reconciles it, so cancelling here loses no accounting.
        tokio::time::timeout_at(walk_deadline, dispatch(route, req))
            .await
            .unwrap_or(Err(BackendError::RoutingDeadlineExceeded))
    } else {
        dispatch(route, req).await
    };

    if let Err(error) = &result {
        tracing::warn!(
            chalk.router.attempt.index = attempt_index as i64,
            chalk.router.attempt.duration_ms = started_at.elapsed().as_millis() as i64,
            chalk.router.attempt.model = %candidate.label,
            llm.system = %route.resolution.provider_kind,
            llm.provider = %route.resolution.provider_kind,
            llm.model_name = %route.resolution.upstream_model_id,
            chalk.connection_id = %route.resolution.connection_id.as_deref().unwrap_or(""),
            chalk.connection_name = %route.resolution.connection_name,
            error.type = backend_error_type(error),
            error.message = %error,
            capability.advertised_mismatch = advertised_support == Support::Supported
                && indicates_capability_mismatch(error),
            "chalk.router.attempt.failed"
        );
    }
    match result {
        Ok(value) => Ok((value, permit)),
        Err(error) => {
            if let Some(permit) = permit {
                if owner.is_none() || definite_background_rejection(&error) {
                    if matches!(error, BackendError::InvalidResponse(_)) {
                        // A full generation may already have occurred.
                        // Settle with unknown usage rather than failed-attempt usage.
                        permit.finish(None).await;
                    } else {
                        permit.finish_failed().await;
                    }
                }
            }
            Err(AttemptFailure {
                error,
                dispatched: true,
            })
        }
    }
}

/// Admit one resolved generation immediately before dispatch. Database errors
/// fail open by design; an exhausted policy returns the local 429 that callers
/// expose without contacting the upstream.
pub(crate) async fn acquire_rate_limit(
    state: &crate::AppState,
    api_key: &ResolvedApiKey,
    resolution: &RouteResolution,
    model: RateLimitModel<'_>,
    token_reservation: TokenReservation,
    failed_attempt_tokens: Option<u64>,
) -> Result<Option<QuotaPermit>, BackendError> {
    acquire_rate_limit_owned(
        state,
        api_key,
        resolution,
        model,
        token_reservation,
        failed_attempt_tokens,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn acquire_rate_limit_owned(
    state: &crate::AppState,
    api_key: &ResolvedApiKey,
    resolution: &RouteResolution,
    model: RateLimitModel<'_>,
    token_reservation: TokenReservation,
    failed_attempt_tokens: Option<u64>,
    owner: Option<&crate::quota::BackgroundOwner>,
) -> Result<Option<QuotaPermit>, BackendError> {
    if !state.quota_backend.enabled() {
        return Ok(None);
    }
    let actions =
        plan_rate_limit_actions(state, api_key, resolution, model, token_reservation).await?;
    admit_route_actions(
        state,
        api_key,
        resolution,
        model,
        &actions,
        failed_attempt_tokens,
        owner,
    )
    .await
}

async fn admit_route_actions(
    state: &crate::AppState,
    api_key: &ResolvedApiKey,
    resolution: &RouteResolution,
    model: RateLimitModel<'_>,
    actions: &[QuotaAction],
    failed_attempt_tokens: Option<u64>,
    owner: Option<&crate::quota::BackgroundOwner>,
) -> Result<Option<QuotaPermit>, BackendError> {
    if actions.is_empty() {
        return Ok(None);
    }
    let decision = match owner {
        Some(owner) => {
            state
                .quota_backend
                .acquire_background(actions, failed_attempt_tokens, owner)
                .await
        }
        None => {
            state
                .quota_backend
                .acquire_many(actions, failed_attempt_tokens)
                .await
        }
    };
    route_admission_result(state, api_key, resolution, model, owner, decision)
}

fn route_admission_result(
    state: &crate::AppState,
    api_key: &ResolvedApiKey,
    resolution: &RouteResolution,
    model: RateLimitModel<'_>,
    owner: Option<&crate::quota::BackgroundOwner>,
    decision: Result<QuotaDecision, crate::quota::QuotaBackendError>,
) -> Result<Option<QuotaPermit>, BackendError> {
    match decision {
        Ok(QuotaDecision::Allowed(grant)) => {
            Ok(grant.map(|grant| QuotaPermit::new(state.quota_backend.clone(), grant)))
        }
        Ok(QuotaDecision::Limited(exceeded)) => {
            tracing::warn!(
                key_prefix = %api_key.0.key_prefix,
                connection_id = resolution.connection_id.as_deref(),
                public_model = match model {
                    RateLimitModel::Exact(public_model)
                    | RateLimitModel::Resolved { public_model_id: public_model, .. } => public_model,
                    RateLimitModel::ProviderSelected => "<provider-selected>",
                    RateLimitModel::NotApplicable => "<not-applicable>",
                },
                source = exceeded.source.as_str(),
                meter = exceeded.meter.as_str(),
                limit_value = exceeded.limit_value,
                retry_after_ms = exceeded.retry_after.as_millis() as u64,
                "request rejected by router quota"
            );
            Err(quota_exceeded_error(exceeded))
        }
        Err(error) => {
            if owner.is_some() {
                return Err(BackendError::api_status(
                    503,
                    "background quota acquisition is temporarily unavailable",
                ));
            }
            if error.is_contended() {
                return Err(quota_contended_error());
            }
            tracing::error!(%error, "route quota acquisition failed; allowing request");
            Ok(None)
        }
    }
}

/// Resolve only the per-route action plan, without charging it. Planning may
/// reject unknown estimates; that rejection still follows request admission.
async fn plan_rate_limit_actions(
    state: &crate::AppState,
    api_key: &ResolvedApiKey,
    resolution: &RouteResolution,
    model: RateLimitModel<'_>,
    token_reservation: TokenReservation,
) -> Result<Vec<QuotaAction>, BackendError> {
    let Some(connection_id) = resolution.connection_id.as_deref() else {
        return Ok(Vec::new());
    };
    let model = match model {
        RateLimitModel::Exact(public_model_id) => RateLimitModel::Resolved {
            public_model_id,
            resolution,
        },
        other => other,
    };
    let plan = state
        .quota_resolver
        .rate_actions(
            &RateLimitMatch {
                key_hash: &api_key.0.key_hash,
                usage_pool_id: api_key.0.usage_pool_id.as_deref(),
                connection_id: Some(connection_id),
                model,
            },
            token_reservation,
        )
        .await
        .map_err(|error| BackendError::Other(error.to_string()))?;
    if plan.requires_token_reservation {
        return Err(BackendError::api_status(400, UNKNOWN_TOKEN_ESTIMATE));
    }
    if plan.requires_public_model {
        return Err(BackendError::api_status(
            400,
            "model is required when model-scoped rate limits apply",
        ));
    }
    Ok(plan.actions)
}

/// Commit request and route admission separately so unrelated phase locks do
/// not overlap. A route denial never refunds request RPM. Each phase fails open
/// independently on backend errors, without replaying ambiguous admissions,
/// except that an exhausted pool skips the route phase and lock contention
/// fails closed.
async fn acquire_first_request_and_route<R: RoutedRequest>(
    state: &crate::AppState,
    api_key: &ResolvedApiKey,
    request: &R,
    candidate: &Candidate,
    reservation: TokenReservation,
    input_tokens: Option<u64>,
) -> Result<
    (
        Option<QuotaPermit>,
        Result<Option<QuotaPermit>, BackendError>,
    ),
    BackendError,
> {
    if !state.quota_backend.enabled() {
        return Ok((None, Ok(None)));
    }
    let request_plan = state
        .quota_resolver
        .request_actions(
            &api_key.0.key_hash,
            api_key.0.usage_pool_id.as_deref(),
            reservation,
            chrono::Utc::now(),
        )
        .await
        .map_err(|error| BackendError::Other(error.to_string()))?;
    if request_plan.requires_token_reservation {
        return Err(BackendError::api_status(400, UNKNOWN_TOKEN_ESTIMATE));
    }
    let (route_tokens, failed_tokens) = request.rate_limit_tokens(&candidate.route, input_tokens);
    let route_actions = match plan_rate_limit_actions(
        state,
        api_key,
        &candidate.route.resolution,
        RateLimitModel::Exact(&candidate.identity),
        TokenReservation::from_option(route_tokens).with_soft_cap(request.soft_cap()),
    )
    .await
    {
        Ok(actions) => actions,
        Err(error) => {
            // Match the original order: request RPM is charged even when the
            // route plan rejects an unknown token estimate before dispatch.
            let permit = admit_request_actions(state, &request_plan.actions, None).await?;
            return Ok((permit, Err(error)));
        }
    };
    let (request_permit, pool_exhausted) = if request_plan.actions.is_empty() {
        (None, false)
    } else {
        let decision = state
            .quota_backend
            .acquire_many(&request_plan.actions, None)
            .await;
        let pool_exhausted = matches!(&decision, Err(error) if error.is_pool_exhausted());
        (
            request_admission_result(state, None, decision)?,
            pool_exhausted,
        )
    };
    if pool_exhausted {
        // Do not queue the route phase behind the pool that just timed out;
        // the request already fails open, so its route admission does too.
        return Ok((request_permit, Ok(None)));
    }
    let route = admit_route_actions(
        state,
        api_key,
        &candidate.route.resolution,
        RateLimitModel::Exact(&candidate.identity),
        &route_actions,
        failed_tokens,
        None,
    )
    .await;
    Ok((request_permit, route))
}

/// Check `route` against `requirements`, tracing the decision and, on
/// failure, formatting the shared `Model '<model>' cannot serve <operation>:
/// <reason>.` 400 (or, when `model` is `None`, `The default model cannot
/// serve ...`). This tail was duplicated verbatim across every
/// "resolve-once, dispatch-through" handler — twice within
/// `/v1/images/generations` alone, for its model-present/model-absent
/// branches.
///
/// Handlers whose surface also needs a permission gate *between* the resolve
/// and this check (embeddings, image generation enforce an API-key
/// allowlist) call this directly, after their own resolve + permission gate,
/// to keep that ordering intact. [`run_passthrough`] wraps resolve + this
/// check for surfaces with no such gate.
pub(crate) fn check_passthrough_capabilities(
    route: &ResolvedRoute,
    model: Option<&str>,
    requirements: &CapabilityRequirements,
    operation: &str,
) -> Result<(), BackendError> {
    let traced_model = model.unwrap_or(route.resolution.upstream_model_id.as_str());
    crate::capabilities::trace_decision(route, traced_model, requirements, operation);
    if let Some(reason) = route.capabilities.profile.first_failure(requirements) {
        let subject = match model {
            Some(model) => format!("Model '{model}'"),
            None => "The default model".to_string(),
        };
        return Err(BackendError::api_status(
            400,
            format!("{subject} cannot serve {operation}: {reason}."),
        ));
    }
    Ok(())
}

/// Resolve once, stamp the selected route, enforce key permissions and then
/// check capabilities before dispatch. Quota acquisition remains the caller's
/// responsibility because reservation semantics differ across surfaces.
pub(crate) async fn run_passthrough(
    state: &crate::AppState,
    api_key: &ResolvedApiKey,
    span: &LlmSpanWriter,
    model: &str,
    requirements: &CapabilityRequirements,
    operation: &str,
) -> Result<ResolvedRoute, BackendError> {
    let route = state.backend.resolve(model).await?;
    span.set_route(&route.resolution);
    api_key
        .0
        .permission
        .allows_route(&route.resolution, model)
        .map_err(|denied| BackendError::PermissionDenied(denied.to_string()))?;
    check_passthrough_capabilities(
        &route,
        Some(route.resolution.upstream_model_id.as_str()),
        requirements,
        operation,
    )?;
    Ok(route)
}

pub(crate) async fn acquire_request_quota(
    state: &crate::AppState,
    api_key: &ResolvedApiKey,
    reservation: TokenReservation,
) -> Result<Option<QuotaPermit>, BackendError> {
    acquire_request_quota_owned(state, api_key, reservation, None).await
}

async fn acquire_request_quota_owned(
    state: &crate::AppState,
    api_key: &ResolvedApiKey,
    reservation: TokenReservation,
    owner: Option<&crate::quota::BackgroundOwner>,
) -> Result<Option<QuotaPermit>, BackendError> {
    if !state.quota_backend.enabled() {
        return Ok(None);
    }
    let plan = state
        .quota_resolver
        .request_actions(
            &api_key.0.key_hash,
            api_key.0.usage_pool_id.as_deref(),
            reservation,
            chrono::Utc::now(),
        )
        .await
        .map_err(|error| BackendError::Other(error.to_string()))?;
    if plan.requires_token_reservation {
        return Err(BackendError::api_status(400, UNKNOWN_TOKEN_ESTIMATE));
    }
    admit_request_actions(state, &plan.actions, owner).await
}

async fn admit_request_actions(
    state: &crate::AppState,
    actions: &[QuotaAction],
    owner: Option<&crate::quota::BackgroundOwner>,
) -> Result<Option<QuotaPermit>, BackendError> {
    if actions.is_empty() {
        return Ok(None);
    }
    let decision = match owner {
        Some(owner) => {
            state
                .quota_backend
                .acquire_background(actions, None, owner)
                .await
        }
        None => state.quota_backend.acquire_many(actions, None).await,
    };
    request_admission_result(state, owner, decision)
}

fn request_admission_result(
    state: &crate::AppState,
    owner: Option<&crate::quota::BackgroundOwner>,
    decision: Result<QuotaDecision, crate::quota::QuotaBackendError>,
) -> Result<Option<QuotaPermit>, BackendError> {
    match decision {
        Ok(QuotaDecision::Allowed(grant)) => {
            Ok(grant.map(|grant| QuotaPermit::new(state.quota_backend.clone(), grant)))
        }
        Ok(QuotaDecision::Limited(exceeded)) => Err(quota_exceeded_error(exceeded)),
        Err(error) => {
            if owner.is_some() {
                return Err(BackendError::api_status(
                    503,
                    "background quota acquisition is temporarily unavailable",
                ));
            }
            if error.is_contended() {
                return Err(quota_contended_error());
            }
            tracing::error!(%error, "request quota acquisition failed; allowing request");
            Ok(None)
        }
    }
}

/// Admit one logical request and its single resolved upstream dispatch.
/// Request-level policies are acquired first; a local rejection refunds only
/// reconcilable token/concurrency state before returning to the caller.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn acquire_single_route_quota(
    state: &crate::AppState,
    api_key: &ResolvedApiKey,
    resolution: &RouteResolution,
    model: RateLimitModel<'_>,
    token_reservation: TokenReservation,
    failed_attempt_tokens: Option<u64>,
) -> Result<Option<QuotaPermit>, BackendError> {
    let request_permit = acquire_request_quota(state, api_key, token_reservation).await?;
    match acquire_rate_limit(
        state,
        api_key,
        resolution,
        model,
        token_reservation,
        failed_attempt_tokens,
    )
    .await
    {
        Ok(route_permit) => Ok(QuotaPermit::merge(request_permit, route_permit)),
        Err(error) => {
            if let Some(permit) = request_permit {
                permit.refund().await;
            }
            Err(error)
        }
    }
}

pub(crate) fn quota_exceeded_error(exceeded: QuotaExceeded) -> BackendError {
    let retry_secs = exceeded.retry_after.as_secs_f64().ceil().max(1.0) as u64;
    let source = exceeded.source.as_str();
    let meter = exceeded.meter.as_str();
    let code = if exceeded.source == QuotaSource::BudgetLimit {
        "budget_exceeded"
    } else {
        "rate_limit_exceeded"
    };
    let mut headers = vec![
        ("retry-after".to_string(), retry_secs.to_string()),
        ("x-chalk-quota-source".to_string(), source.to_string()),
        ("x-chalk-quota-meter".to_string(), meter.to_string()),
        (
            "x-chalk-quota-limit".to_string(),
            exceeded.limit_value.to_string(),
        ),
        ("x-chalk-error-code".to_string(), code.to_string()),
    ];
    if exceeded.source == QuotaSource::RateLimit {
        let limit_type = match exceeded.meter {
            crate::quota::QuotaMeter::Requests => "requests_per_minute",
            crate::quota::QuotaMeter::Tokens => "tokens_per_minute",
            crate::quota::QuotaMeter::ConcurrentRequests => "concurrent_requests",
        };
        headers.extend([
            ("x-chalk-ratelimit-type".to_string(), limit_type.to_string()),
            (
                "x-chalk-ratelimit-limit".to_string(),
                exceeded.limit_value.to_string(),
            ),
        ]);
    }
    BackendError::api_error(
        format!(
            "{source} {meter} limit of {} exceeded",
            exceeded.limit_value
        ),
        429,
        headers,
    )
}

/// Admission waited `lock_timeout` on a quota row another request held. Fail
/// closed: failing open would exempt exactly the most contended policy.
pub(crate) fn quota_contended_error() -> BackendError {
    BackendError::api_error(
        "quota is temporarily contended by concurrent requests; retry shortly",
        429,
        vec![
            ("retry-after".to_string(), "1".to_string()),
            (
                "x-chalk-error-code".to_string(),
                "quota_contended".to_string(),
            ),
        ],
    )
}

struct AttemptFailure {
    error: BackendError,
    dispatched: bool,
}

/// A retryable raw upstream response retained while the fallback chain walks.
/// If every candidate fails, the most recent response is forwarded exactly
/// instead of being replaced with a router-owned error envelope.
pub(crate) struct RawUpstreamFailure {
    status: StatusCode,
    content_type: String,
    headers: Vec<(String, String)>,
    body: Bytes,
}

/// Turn retryable raw HTTP statuses (429/5xx) into a dispatch error so the
/// shared fallback walker advances. Successful and other 4xx responses remain
/// terminal raw responses. Transport errors clear a stale response from an
/// earlier candidate.
pub(crate) async fn handle_raw_upstream_dispatch(
    result: Result<reqwest::Response, BackendError>,
    raw_failure: &std::sync::Mutex<Option<RawUpstreamFailure>>,
) -> Result<reqwest::Response, BackendError> {
    let response = match result {
        Ok(response) => response,
        Err(error) => {
            *raw_failure.lock().unwrap() = None;
            return Err(error);
        }
    };

    let status = response.status();
    if status.is_success()
        || (status != reqwest::StatusCode::TOO_MANY_REQUESTS && !status.is_server_error())
    {
        return Ok(response);
    }

    let headers = forwardable_upstream_headers(response.headers());
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let status_code =
        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = response.bytes().await.unwrap_or_default();
    let message = String::from_utf8_lossy(&body).into_owned();

    *raw_failure.lock().unwrap() = Some(RawUpstreamFailure {
        status: status_code,
        content_type,
        headers: headers.clone(),
        body,
    });
    Err(BackendError::api_error(message, status.as_u16(), headers))
}

pub(crate) fn raw_upstream_failure_response(raw: RawUpstreamFailure) -> Response {
    let mut builder = Response::builder()
        .status(raw.status)
        .header(header::CONTENT_TYPE, &raw.content_type);
    for (name, value) in &raw.headers {
        builder = builder.header(name, value);
    }
    let mut response = builder.body(Body::from(raw.body)).unwrap();
    crate::handlers::stamp_trace_headers(response.headers_mut());
    response
}

/// Forward a terminal raw upstream response with its exact status, content
/// type, and body plus the shared safe response-header allowlist.
pub(crate) async fn raw_upstream_response(response: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(response.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let headers = forwardable_upstream_headers(response.headers());
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let body = response.bytes().await.unwrap_or_default();
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type);
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    let mut response = builder.body(Body::from(body)).unwrap();
    crate::handlers::stamp_trace_headers(response.headers_mut());
    response
}

/// How long the fallback walk keeps starting new attempts. It never cancels a
/// foreground attempt already in flight: a healthy long generation must be
/// allowed to finish and settle its usage. Background submissions are bounded
/// by the same instant inside [`dispatch_attempt`].
const ROUTING_WALK_DEADLINE: Duration = Duration::from_secs(300);

const NO_AVAILABLE_ROUTE: &str = "no destination configured for this model is available";

/// Who chose a plan's fallback list, which decides the public identity its
/// destinations are authorized and rate-limited under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FallbackOwner {
    /// Configured on the requested `chalk/*` route (grid or judge outcome).
    /// Part of the route's definition, like its default destination.
    Route,
    /// Supplied by the caller in the request body. Each destination is
    /// addressed as if the caller had requested it directly.
    Caller,
}

/// The destinations that may serve one logical request, in attempt order.
pub(crate) struct RoutePlan {
    /// The public model the caller requested (`chalk/*` or a direct model id).
    /// The primary and route-owned fallbacks are authorized and rate-limited
    /// under it.
    pub requested_model: String,
    /// Id of the model route `requested_model` names, `None` for a direct id.
    /// A judge selection or route-owned fallback resolves to a concrete
    /// target that carries no route id, so its grant is checked against this.
    pub requested_route_id: Option<String>,
    /// Label for the primary: the requested model, or the concrete target a
    /// route judge selected.
    pub primary_model: String,
    /// `None` when a judge-selected target did not resolve; its fallbacks
    /// still run.
    pub primary: Option<ResolvedRoute>,
    pub fallbacks: Vec<String>,
    pub fallback_owner: FallbackOwner,
}

impl RoutePlan {
    /// Serve `primary` with the fallbacks its route configures for it.
    pub(crate) fn configured(requested_model: String, primary: ResolvedRoute) -> Self {
        let fallbacks = primary
            .routing
            .as_ref()
            .and_then(|grid| grid.fallbacks.get(&primary.concrete_model()))
            .cloned()
            .unwrap_or_default();
        Self {
            primary_model: requested_model.clone(),
            requested_model,
            requested_route_id: primary.resolution.model_route_id.clone(),
            primary: Some(primary),
            fallbacks,
            fallback_owner: FallbackOwner::Route,
        }
    }

    pub(crate) fn without_fallbacks(mut self) -> Self {
        self.fallbacks.clear();
        self
    }
}

/// One destination the walk will attempt.
struct Candidate {
    /// Requested model or concrete target, reported in headers and logs.
    label: String,
    /// Public model id this attempt is authorized and rate-limited under.
    identity: String,
    route: ResolvedRoute,
    is_fallback: bool,
}

/// Why a destination was left out of the plan before dispatch.
enum Skip {
    Denied(String),
    Ineligible(String),
}

/// Dispatch the plan's primary, then each fallback.
///
/// The caller resolves `plan.primary` exactly once. Dispatch goes directly to
/// that leaf backend; it never re-enters the router against a newer snapshot.
/// Each fallback is resolved once and separately authorized: its provider and
/// connection always, and its model under the identity [`RoutePlan`] assigns,
/// so a caller-chosen fallback cannot escape the presented key's allowlist.
///
/// Candidates that cannot be resolved, authorized, or represented losslessly
/// are *skipped*, not dispatched. A skip never replaces the last real dispatch
/// error: otherwise a typo or capability mismatch could replace an upstream 429
/// (and its `Retry-After`) with an unrelated local error. Only dispatched
/// failures advance the configured chain and participate in attempt counts.
///
/// Streaming callers run this entire loop before committing a downstream 200 or
/// any bytes. Every adapter validates upstream status before returning its body,
/// making fallback a retry-before-first-byte operation (RFP-0002 §5.1).
pub(crate) async fn try_with_fallbacks<R, T, F, Fut>(
    span: &LlmSpanWriter,
    state: &crate::AppState,
    req: R,
    plan: RoutePlan,
    api_key: &ResolvedApiKey,
    input_tokens: Option<u64>,
    dispatch: F,
) -> Result<(T, Option<String>, RouteResolution, Option<QuotaPermit>), BackendError>
where
    R: RoutedRequest,
    F: Fn(Arc<dyn BackendProvider>, R) -> Fut,
    Fut: std::future::Future<Output = Result<T, BackendError>>,
{
    try_with_fallbacks_if(
        span,
        state,
        req,
        plan,
        api_key,
        input_tokens,
        |route, request| request.check_capability(route),
        move |route, request| dispatch(route.backend.clone(), request),
    )
    .await
}

/// Availability failures and explicitly classified reasoning incompatibility.
/// Decode, configuration, and unclassified non-429 client errors remain
/// terminal. This predicate runs before the response is committed.
pub(crate) fn allows_fallback(error: &BackendError) -> bool {
    match error {
        BackendError::ApiError {
            status,
            reasoning_incompatible,
            ..
        } => {
            *status == 429
                || (500..600).contains(status)
                || (*status == 400 && *reasoning_incompatible)
        }
        BackendError::RequestError(error) => {
            error.is_connect()
                || error.is_timeout()
                || (error.is_request() && !error.is_builder() && !error.is_decode())
        }
        _ => false,
    }
}

/// Variant of [`try_with_fallbacks`] that can skip any candidate, including the
/// primary, before dispatch when that backend cannot represent the request
/// losslessly. The eligibility error is safe to return when no candidate can
/// serve, so callers can identify the unsupported field or capability without
/// logging request content.
///
/// `input_tokens` is prepared once by the caller alongside usage fallback
/// accounting. `None` remains unknown for every candidate in the quota fold.
///
/// When nothing survives planning, the response says why: a capability
/// rejection is a 400, a key that may not reach any destination is a 403, and
/// destinations that no longer resolve are a 503.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn try_with_fallbacks_if<R, T, E, F, Fut>(
    span: &LlmSpanWriter,
    state: &crate::AppState,
    req: R,
    plan: RoutePlan,
    api_key: &ResolvedApiKey,
    input_tokens: Option<u64>,
    eligible: E,
    dispatch: F,
) -> Result<(T, Option<String>, RouteResolution, Option<QuotaPermit>), BackendError>
where
    R: RoutedRequest,
    E: Fn(&ResolvedRoute, &R) -> Result<(), String>,
    F: Fn(&ResolvedRoute, R) -> Fut,
    Fut: std::future::Future<Output = Result<T, BackendError>>,
{
    let walk_deadline = tokio::time::Instant::now() + ROUTING_WALK_DEADLINE;
    let RoutePlan {
        requested_model,
        requested_route_id,
        primary_model,
        primary,
        fallbacks,
        fallback_owner,
    } = plan;
    if fallbacks.len() > crate::route_grid::MAX_ALTERNATIVES {
        return Err(BackendError::api_status(
            400,
            "too many fallback alternatives",
        ));
    }
    // A destination authorized under the requested model is part of the
    // requested route, whatever concrete target it resolved to.
    let check = |route: &ResolvedRoute, identity: &str| -> Result<(), Skip> {
        let route_id = if identity == requested_model {
            requested_route_id.as_deref()
        } else {
            route.resolution.model_route_id.as_deref()
        };
        api_key
            .0
            .permission
            .allows_model(&route.resolution, identity, route_id)
            .map_err(|error| Skip::Denied(error.to_string()))?;
        eligible(route, &req).map_err(Skip::Ineligible)
    };
    let mut planned = Vec::new();
    let mut rejection = None;
    let mut denied = None;
    let mut record = |model: &str, skip: Skip| match skip {
        Skip::Denied(reason) => {
            tracing::warn!(model, %reason, "destination denied by API key permission; skipping");
            denied.get_or_insert(reason);
        }
        Skip::Ineligible(reason) => {
            tracing::warn!(model, %reason, "destination cannot represent request; skipping");
            rejection.get_or_insert(reason);
        }
    };
    // Only a `chalk/*` route owns other destinations. A direct model id grants
    // access to itself alone, so anything else it reaches uses its own id.
    let requested_is_route = requested_model.starts_with(ROUTE_RESERVED_PREFIX);
    let mut seen = std::collections::HashSet::new();
    if let Some(route) = primary {
        seen.insert(route.concrete_model());
        match check(&route, &requested_model) {
            Ok(()) => planned.push(Candidate {
                label: primary_model,
                identity: requested_model.clone(),
                route,
                is_fallback: false,
            }),
            Err(skip) => record(&primary_model, skip),
        }
    }
    for model in fallbacks {
        let route = match state.backend.resolve_concrete(&model).await {
            Ok(route) => route,
            Err(error) => {
                tracing::warn!(model, %error, "fallback target did not resolve; skipping");
                continue;
            }
        };
        if !seen.insert(route.concrete_model()) {
            continue;
        }
        let identity = if fallback_owner == FallbackOwner::Route && requested_is_route {
            requested_model.clone()
        } else {
            match state.backend.direct_model_id(&model).await {
                Some(identity) => identity,
                None => {
                    record(
                        &model,
                        Skip::Denied(format!(
                            "fallback '{model}' is reachable only through a model route"
                        )),
                    );
                    continue;
                }
            }
        };
        match check(&route, &identity) {
            Ok(()) => planned.push(Candidate {
                label: model,
                identity,
                route,
                is_fallback: true,
            }),
            Err(skip) => record(&model, skip),
        }
    }
    if planned.is_empty() {
        span.set_attempt_summary(0, false);
        return Err(if let Some(reason) = rejection {
            BackendError::api_status(400, reason)
        } else if let Some(reason) = denied {
            BackendError::PermissionDenied(reason)
        } else {
            BackendError::api_status(503, NO_AVAILABLE_ROUTE)
        });
    }
    // Preserve unknown reservations in the conservative fold. All eligible
    // destinations are weighed before any logical-request quota is reserved.
    let reservation = TokenReservation::conservative_max(
        planned
            .iter()
            .map(|candidate| req.rate_limit_tokens(&candidate.route, input_tokens).0),
    )
    .with_soft_cap(req.soft_cap());

    let owner = req
        .background_accounting_id()
        .map(|event_id| crate::quota::BackgroundOwner {
            environment_id: state.environment_id.clone(),
            event_id,
        });

    let (mut request_permit, mut first_admission) = if owner.is_some() {
        (
            acquire_request_quota_owned(state, api_key, reservation, owner.as_ref()).await?,
            None,
        )
    } else {
        let (request_permit, route_admission) = acquire_first_request_and_route(
            state,
            api_key,
            &req,
            &planned[0],
            reservation,
            input_tokens,
        )
        .await?;
        (request_permit, Some(route_admission))
    };

    let mut attempt_count = 0;
    let mut last_dispatch_error = None;
    let mut last_rate_limit_error = None;
    let mut planned = planned.into_iter().peekable();
    let mut req = Some(req);
    while let Some(candidate) = planned.next() {
        if attempt_count > 0 && tokio::time::Instant::now() >= walk_deadline {
            tracing::warn!(
                %requested_model,
                attempts = attempt_count,
                "routing deadline reached; not starting further fallbacks"
            );
            break;
        }
        // Earlier attempts retain an original request for fallbacks. The final
        // candidate (usually the only one) can consume it without a deep copy.
        let attempt_request = if planned.peek().is_none() {
            req.take().expect("last candidate owns the request")
        } else {
            req.as_ref().expect("request retained for fallback").clone()
        };
        match dispatch_attempt(
            state,
            api_key,
            &candidate,
            attempt_request,
            attempt_count,
            input_tokens,
            walk_deadline,
            first_admission.take(),
            &dispatch,
        )
        .await
        {
            Ok((value, permit)) => {
                attempt_count += 1;
                span.set_attempt_summary(attempt_count, candidate.is_fallback);
                return Ok((
                    value,
                    candidate.is_fallback.then_some(candidate.label),
                    candidate.route.resolution,
                    QuotaPermit::merge(request_permit.take(), permit),
                ));
            }
            Err(failure) => {
                if owner.is_some()
                    && failure.dispatched
                    && !definite_background_rejection(&failure.error)
                {
                    // The upstream job may still be running. Do not retry
                    // or finalize its background-owned quota here.
                    span.set_attempt_summary(attempt_count + 1, false);
                    return Err(failure.error);
                }

                let retryable = allows_fallback(&failure.error);
                if failure.dispatched {
                    attempt_count += 1;
                    last_dispatch_error = Some(failure.error);
                } else if !retryable {
                    last_dispatch_error = Some(failure.error);
                } else {
                    last_rate_limit_error = Some(failure.error);
                }
                if !retryable {
                    break;
                }
            }
        }
    }
    span.set_attempt_summary(attempt_count, false);
    if let Some(permit) = request_permit.take() {
        if attempt_count > 0 {
            permit.finish(None).await;
        } else {
            permit.refund().await;
        }
    }
    Err(last_dispatch_error
        .or(last_rate_limit_error)
        .unwrap_or_else(|| BackendError::api_status(400, NO_LOSSLESS_ROUTE)))
}

/// Stamp the actual route selected for a successful downstream response.
pub(crate) fn stamp_routing_headers(
    headers: &mut HeaderMap,
    requested_model: &str,
    served_model: &str,
    fallback_model: Option<&str>,
) {
    if served_model != requested_model {
        if let Ok(value) = HeaderValue::from_str(served_model) {
            headers.insert("x-chalk-routed-model", value);
        }
    }
    if let Some(fallback_model) = fallback_model {
        if let Ok(value) = HeaderValue::from_str(fallback_model) {
            headers.insert("x-fallback-model", value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_deadline_is_not_a_provider_rejection_or_retryable_failure() {
        let deadline = BackendError::RoutingDeadlineExceeded;
        assert!(!definite_background_rejection(&deadline));
        assert!(!allows_fallback(&deadline));
        assert_eq!(
            super::super::backend_error_response(&deadline).status(),
            StatusCode::GATEWAY_TIMEOUT
        );

        let upstream_timeout = BackendError::api_status(504, "upstream gateway timeout");
        assert!(definite_background_rejection(&upstream_timeout));
        assert!(allows_fallback(&upstream_timeout));
    }

    #[test]
    fn chat_fallback_requirements_keep_unknown_permissive_and_reject_unsupported() {
        let request: crate::types::ChatCompletionRequest =
            serde_json::from_value(serde_json::json!({
                "model": "custom/model",
                "stream": true,
                "messages": [{
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "describe"},
                        {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}
                    ]
                }],
                "tools": [{
                    "type": "function",
                    "function": {"name": "lookup", "parameters": {"type": "object"}}
                }]
            }))
            .unwrap();
        let requirements = chat_requirements(&request);
        let mut profile = crate::capabilities::unknown_generation_capabilities("model");
        assert!(profile.satisfies(&requirements));

        profile.modalities.input.insert(
            InputModality::Image,
            crate::capabilities::Support::Unsupported,
        );
        assert!(!profile.satisfies(&requirements));
    }

    #[test]
    fn routing_headers_describe_only_an_actual_route_change() {
        let mut headers = HeaderMap::new();
        stamp_routing_headers(&mut headers, "chalk/model", "chalk/model", None);
        assert!(headers.get("x-chalk-routed-model").is_none());
        assert!(headers.get("x-fallback-model").is_none());

        stamp_routing_headers(
            &mut headers,
            "chalk/model",
            "openai/gpt-5",
            Some("openai/gpt-5"),
        );
        assert_eq!(headers["x-chalk-routed-model"], "openai/gpt-5");
        assert_eq!(headers["x-fallback-model"], "openai/gpt-5");
    }

    #[tokio::test]
    async fn compatibility_generation_deadline_is_a_gateway_timeout() {
        let error = response_with_deadline(Duration::from_millis(1), async {
            std::future::pending::<Result<(), BackendError>>().await
        })
        .await
        .unwrap_err();

        assert!(matches!(error, BackendError::ApiError { status: 504, .. }));
    }
}
