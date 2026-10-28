//! OpenAI Responses API frontend.
//!
//! Native Responses preserve JSON/SSE bytes apart from response locator ID
//! tokens, together with upstream success status, MIME, and permitted headers.
//! Providers with only Chat Completions support use the
//! conservative compatibility codec in `crate::responses`; unsupported fields
//! are rejected rather than silently discarded.

use crate::tenancy::RequestState as State;
use std::borrow::Cow;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::{
    body::Body,
    extract::{Json, Path, Query},
    http::{header, HeaderMap},
    response::Response,
};
use bytes::Bytes;
use futures::Stream;
use serde_json::Value;

use crate::adapters::responses_chat::{create_response_stream_via_chat, create_response_via_chat};
use crate::backend::provider::{BackendSseResponse, ResponsesRequestHeaders};
use crate::backend::{BackendError, SseByteStream};
use crate::capabilities::{
    CapabilityRequirements, DeliveryMode, EndpointFeature, InputModality, ModelFeature,
    ModelSurface, OutputModality, Protocol, ProtocolOperation, Support,
};
use crate::middleware::ResolvedApiKey;
use crate::quota::{QuotaPermit, TokenReservation};
use crate::ratelimit::RateLimitModel;
use crate::responses::ResponsesRequest;

use super::routing::{
    acquire_single_route_quota, compat_response_with_deadline, open_stream_with_deadline,
    route_check_native_chat, set_route_span_attributes, stamp_routing_headers,
    try_with_fallbacks_if,
};
use super::sse::{SseDecoder, SseFrame};
use super::trace_attributes::{
    ContentDirection, LlmSpanKind, LlmSpanWriter, MAX_INVOCATION_PARAMETERS_BYTES,
};
use super::usage::{self, TokenUsage, UsageAttribution, UsageWriteFuture};
use crate::usage_estimator::{NormalizedUsageInput, ToolContext, UsageEstimateState};

/// Create a response, preserving the caller's request shape for native
/// backends and translating only when the compatibility codec proves that the
/// request is lossless.
pub async fn create_response(
    State(state): State<crate::AppState>,
    api_key: ResolvedApiKey,
    headers: HeaderMap,
    Json(request): Json<ResponsesRequest>,
) -> Response {
    let start = Instant::now();
    let streaming = request.stream();
    let span = tracing::info_span!("responses", otel.name = "responses.create",);
    let span = LlmSpanWriter::new(span, state.object_store.clone(), LlmSpanKind::Llm);
    super::stamp_authenticated_principal(&span, &api_key);

    span.clone()
        .instrument(async move {
            set_input_span_attributes(&span, &request);
            span.set_stream(streaming);

            let original_model = request.model().to_string();
            let request_headers = ResponsesRequestHeaders::from_header_map(&headers);
            if streaming {
                create_streaming(
                    span,
                    state,
                    api_key,
                    request,
                    original_model,
                    request_headers,
                    start,
                )
                .await
            } else {
                create_non_streaming(
                    span,
                    state,
                    api_key,
                    request,
                    original_model,
                    request_headers,
                )
                .await
            }
        })
        .await
}

#[allow(clippy::result_large_err)] // axum Response is the HTTP error body
async fn resolve_authorized_primary(
    span: &LlmSpanWriter,
    state: &crate::AppState,
    api_key: &ResolvedApiKey,
    original_model: &str,
) -> Result<crate::backend::ResolvedRoute, Response> {
    let route = state
        .backend
        .resolve(original_model)
        .await
        .map_err(|error| crate::handlers::backend_error_response(&error))?;

    set_route_span_attributes(span, &route.resolution);
    if api_key.0.permission.is_restricted() {
        api_key
            .0
            .permission
            .allows_route(&route.resolution, original_model)
            .map_err(|denied| crate::handlers::forbidden_response(denied.to_string()))?;
    }
    Ok(route)
}

fn responses_create_mode(route: &crate::backend::ResolvedRoute) -> DeliveryMode {
    route
        .capabilities
        .profile
        .operation(Protocol::OpenaiResponses, ProtocolOperation::Create)
        .map_or(DeliveryMode::Unknown, |operation| operation.mode)
}

fn check_translated_responses(
    route: &crate::backend::ResolvedRoute,
    request: &ResponsesRequest,
) -> Result<(), String> {
    let chat = request
        .to_chat_completion()
        .map_err(|error| error.to_string())?;
    let native = crate::native_chat::NativeChatRequest::from_typed(chat)
        .map_err(|error| error.to_string())?;
    route_check_native_chat(route, &native)
}

fn responses_request_is_eligible(
    route: &crate::backend::ResolvedRoute,
    request: &ResponsesRequest,
) -> Result<(), String> {
    if request.background() {
        let create = route
            .capabilities
            .profile
            .operation(Protocol::OpenaiResponses, ProtocolOperation::Create);
        let retrieve = route
            .capabilities
            .profile
            .operation(Protocol::OpenaiResponses, ProtocolOperation::Retrieve);
        if !create.is_some_and(|operation| {
            matches!(operation.mode, DeliveryMode::Native | DeliveryMode::Adapted)
                && operation.features.get(&EndpointFeature::Background) == Some(&Support::Supported)
                && (!request.stream()
                    || operation.features.get(&EndpointFeature::Streaming)
                        == Some(&Support::Supported))
        }) || !retrieve.is_some_and(|operation| {
            matches!(operation.mode, DeliveryMode::Native | DeliveryMode::Adapted)
        }) {
            return Err("background Responses require explicit native/adapted background and retrieval support".to_string());
        }
    }
    let mode_supported = match responses_create_mode(route) {
        DeliveryMode::Native | DeliveryMode::Adapted => Ok(()),
        DeliveryMode::Translated | DeliveryMode::Unknown => {
            check_translated_responses(route, request)
        }
        DeliveryMode::Unsupported => {
            Err("Responses create is unsupported for this route".to_string())
        }
    };
    let requirements = responses_requirements(request);
    crate::capabilities::trace_decision(
        route,
        request.model(),
        &requirements,
        "openai_responses.create",
    );
    mode_supported?;
    match route.capabilities.profile.first_failure(&requirements) {
        Some(reason) => Err(reason),
        None => Ok(()),
    }
}

/// Require reasoning capability for requested reasoning, excluding disabled or absent controls.
pub(crate) fn responses_requirements(request: &ResponsesRequest) -> CapabilityRequirements {
    let mut requirements =
        CapabilityRequirements::for_operation(Protocol::OpenaiResponses, ProtocolOperation::Create)
            .surface(ModelSurface::Chat)
            .output(OutputModality::Text);
    for feature in required_endpoint_features(request) {
        requirements = requirements.endpoint_feature(feature);
    }
    let raw = request.raw();
    if raw.get("input").is_some_and(|input| {
        input.is_string() || value_contains_type(Some(input), &["input_text", "text"])
    }) {
        requirements = requirements.input(InputModality::Text);
    }
    if raw
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool.get("type").and_then(Value::as_str) == Some("function"))
        })
    {
        requirements = requirements.feature(ModelFeature::ToolCalling);
    }
    if value_contains_type(raw.get("input"), &["input_image", "image_url"]) {
        requirements = requirements.input(InputModality::Image);
    }
    if raw
        .pointer("/text/format/type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind != "text")
    {
        requirements = requirements.feature(ModelFeature::StructuredOutput);
    }
    if raw.get("reasoning").is_some_and(|value| {
        crate::translation::responses_chat::reasoning_effort(value)
            .map_or(true, |effort| effort.is_some_and(|effort| effort != "none"))
            || value.as_object().is_some_and(|fields| {
                fields
                    .iter()
                    .any(|(name, value)| name != "effort" && !value.is_null())
            })
    }) {
        requirements = requirements.feature(ModelFeature::Reasoning);
    }
    if raw.get("parallel_tool_calls").and_then(Value::as_bool) == Some(true) {
        requirements = requirements.feature(ModelFeature::ParallelToolCalling);
    }
    if raw
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool.get("strict").and_then(Value::as_bool) == Some(true))
        })
    {
        requirements = requirements.feature(ModelFeature::StrictToolSchema);
    }
    if value_contains_type(raw.get("tools"), &["computer_use_preview", "computer_use"]) {
        requirements = requirements.feature(ModelFeature::ComputerUse);
    }
    requirements
}

fn value_contains_type(value: Option<&Value>, kinds: &[&str]) -> bool {
    match value {
        Some(Value::Array(values)) => values
            .iter()
            .any(|value| value_contains_type(Some(value), kinds)),
        Some(Value::Object(fields)) => {
            fields
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kinds.contains(&kind))
                || fields
                    .values()
                    .any(|value| value_contains_type(Some(value), kinds))
        }
        _ => false,
    }
}

fn required_endpoint_features(request: &ResponsesRequest) -> Vec<EndpointFeature> {
    let mut required = Vec::new();
    if request.raw().get("stream").and_then(Value::as_bool) == Some(true) {
        required.push(EndpointFeature::Streaming);
    }
    if request.previous_response_id().is_some() {
        required.push(EndpointFeature::StoredContinuation);
    }
    if request.raw().get("background").and_then(Value::as_bool) == Some(true) {
        required.push(EndpointFeature::Background);
    }
    if let Some(tools) = request.raw().get("tools").and_then(Value::as_array) {
        for tool in tools {
            match tool.get("type").and_then(Value::as_str) {
                Some("function") => required.push(EndpointFeature::ToolCalling),
                Some(kind) if kind.starts_with("web_search") => {
                    required.push(EndpointFeature::ServerToolWebSearch)
                }
                Some("file_search") => required.push(EndpointFeature::ServerToolFileSearch),
                Some("code_interpreter") => {
                    required.push(EndpointFeature::ServerToolCodeInterpreter)
                }
                Some(kind) if kind.starts_with("computer_use") => {
                    required.push(EndpointFeature::ServerToolComputerUse)
                }
                Some("mcp") => required.push(EndpointFeature::ServerToolMcp),
                _ => {}
            }
        }
    }
    if value_contains_type(request.raw().get("input"), &["input_image", "image_url"]) {
        required.push(EndpointFeature::ImageInput);
    }
    required.sort_unstable();
    required.dedup();
    required
}

fn locator_matches_route(
    codec: &crate::response_locator::ResponseLocatorCodec,
    route: &crate::backend::ResolvedRoute,
    request: &ResponsesRequest,
) -> Result<(), String> {
    let Some(response_id) = request.previous_response_id() else {
        return Ok(());
    };
    if !crate::response_locator::ResponseLocatorCodec::is_router_locator(response_id) {
        if codec.is_environment_bound() {
            return Err("cluster routers require an environment-bound previous_response_id".into());
        }
        return Ok(());
    }
    let locator = codec
        .decode(response_id)
        .map_err(|error| error.to_string())?;
    if route.resolution.connection_id.as_deref() == Some(locator.connection_id.as_str()) {
        Ok(())
    } else {
        Err("previous_response_id belongs to a different provider connection".to_string())
    }
}

fn prepare_native_response_request(
    codec: &crate::response_locator::ResponseLocatorCodec,
    route: &crate::backend::ResolvedRoute,
    request: &mut ResponsesRequest,
) -> Result<(), BackendError> {
    let Some(response_id) = request.previous_response_id().map(str::to_owned) else {
        return Ok(());
    };
    if !crate::response_locator::ResponseLocatorCodec::is_router_locator(&response_id) {
        if codec.is_environment_bound() {
            return Err(BackendError::api_status(
                400,
                "cluster routers require an environment-bound previous_response_id",
            ));
        }
        return Ok(());
    }
    let locator = codec
        .decode(&response_id)
        .map_err(|error| BackendError::api_status(400, error.to_string()))?;
    if route.resolution.connection_id.as_deref() != Some(locator.connection_id.as_str()) {
        return Err(BackendError::api_status(
            400,
            "previous_response_id belongs to a different provider connection",
        ));
    }
    request.set_previous_response_id(locator.upstream_response_id);
    Ok(())
}

/// `model_route_id` is the route the response was created through, recorded
/// so a key granted that route can retrieve it.
fn wrap_response_id(
    codec: &crate::response_locator::ResponseLocatorCodec,
    connection_id: Option<&str>,
    model_route_id: Option<&str>,
    upstream: &mut crate::backend::provider::BackendJsonResponse,
) -> Result<(), BackendError> {
    let (Some(connection_id), Some(upstream_id)) = (
        connection_id,
        upstream
            .body
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned),
    ) else {
        return Ok(());
    };
    let raw = std::str::from_utf8(&upstream.response.body)
        .map_err(|error| BackendError::Other(error.to_string()))?;
    if let Some(rewritten) =
        crate::schema_json::SchemaJson::rewrite_string_members(raw, &[&["id"]], |id| {
            codec.encode(connection_id, model_route_id, id)
        })
        .map_err(|error| BackendError::Other(error.to_string()))?
    {
        upstream.response.body = rewritten.into();
        upstream.body["id"] = codec
            .encode(connection_id, model_route_id, &upstream_id)
            .into();
    }
    Ok(())
}

fn rewrite_locator_frame(
    frame: SseFrame,
    codec: &crate::response_locator::ResponseLocatorCodec,
    connection_id: &str,
    model_route_id: Option<&str>,
) -> Bytes {
    match crate::schema_json::SchemaJson::rewrite_string_members(
        &frame.data,
        &[&["response", "id"], &["response_id"]],
        |id| codec.encode(connection_id, model_route_id, id),
    ) {
        Ok(Some(data)) => frame.replace_data(&data),
        _ => frame.wire,
    }
}

/// Rewrite a batch of frames decoded from one upstream chunk and concatenate
/// them back into a single buffer. SSE clients (and this codebase's own
/// [`SseDecoder`]) only care about the concatenated byte content, not the
/// chunk boundaries the transport happened to deliver, so frames are rewritten
/// and rejoined per-chunk rather than emitted one at a time.
fn rewrite_locator_frames(
    frames: &[SseFrame],
    codec: &crate::response_locator::ResponseLocatorCodec,
    connection_id: &str,
    model_route_id: Option<&str>,
) -> Bytes {
    let mut encoded = Vec::new();
    for frame in frames {
        encoded.extend_from_slice(&rewrite_locator_frame(
            frame.clone(),
            codec,
            connection_id,
            model_route_id,
        ));
    }
    Bytes::from(encoded)
}

/// Locator state captured once the winning route resolves, letting
/// [`ResponsesSseTap`] rewrite the upstream response id into the router's
/// opaque `resp_chalk_...` form using the frames it already decodes for
/// telemetry, instead of running a second SSE decode pass over the same byte
/// stream.
struct ResponseLocatorRewrite {
    codec: crate::response_locator::ResponseLocatorCodec,
    connection_id: String,
    model_route_id: Option<String>,
}

async fn observe_background_frames(
    state: &crate::AppState,
    connection_id: &str,
    event_id: Option<uuid::Uuid>,
    frames: &[SseFrame],
) -> Result<bool, BackendError> {
    let mut accepted = false;
    for frame in frames {
        let Ok(value) = serde_json::from_str::<Value>(&frame.data) else {
            continue;
        };
        let response = value.get("response").unwrap_or(&value);
        if response.get("id").and_then(Value::as_str).is_none() {
            continue;
        }
        if let Some(event_id) = event_id {
            state
                .background_responses
                .accept(event_id, response)
                .await?;
            state.background_responses.settle(state, event_id).await?;
        } else {
            state
                .background_responses
                .observe(state, connection_id, response)
                .await?;
        }
        accepted = true;
    }
    Ok(accepted)
}

async fn track_background_stream(
    state: crate::AppState,
    connection_id: String,
    event_id: Option<uuid::Uuid>,
    mut inner: SseByteStream,
) -> Result<SseByteStream, BackendError> {
    use futures::StreamExt;
    let mut decoder = SseDecoder::default();
    let mut buffered = Vec::new();
    if event_id.is_some() {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let mut bytes_seen = 0usize;
            loop {
                let bytes = inner.next().await.ok_or_else(|| {
                    BackendError::api_status(
                        502,
                        "background stream ended before response identity",
                    )
                })??;
                bytes_seen = bytes_seen.saturating_add(bytes.len());
                if bytes_seen > 128 * 1024 {
                    return Err(BackendError::api_status(
                        502,
                        "background stream identity exceeded bootstrap limit",
                    ));
                }
                let accepted = observe_background_frames(
                    &state,
                    &connection_id,
                    event_id,
                    &decoder.push(&bytes),
                )
                .await?;
                buffered.push(Ok(bytes));
                if accepted {
                    return Ok(());
                }
            }
        })
        .await
        .map_err(|_| BackendError::api_status(504, "background stream identity timed out"))??;
    }
    let tracked = futures::stream::unfold(
        (inner, decoder, state, connection_id, event_id, false),
        |(mut inner, mut decoder, state, connection_id, event_id, ended)| async move {
            if ended {
                return None;
            }
            match inner.next().await {
                Some(Ok(bytes)) => {
                    if let Err(error) = observe_background_frames(
                        &state,
                        &connection_id,
                        event_id,
                        &decoder.push(&bytes),
                    )
                    .await
                    {
                        tracing::error!(%error, "background stream observation will be reconciled by polling");
                    }
                    Some((
                        Ok(bytes),
                        (inner, decoder, state, connection_id, event_id, false),
                    ))
                }
                Some(Err(error)) => Some((
                    Err(error),
                    (inner, decoder, state, connection_id, event_id, true),
                )),
                None => {
                    if let Err(error) = observe_background_frames(
                        &state,
                        &connection_id,
                        event_id,
                        &decoder.finish(),
                    )
                    .await
                    {
                        tracing::error!(%error, "background stream final observation failed");
                    }
                    None
                }
            }
        },
    );
    Ok(Box::pin(futures::stream::iter(buffered).chain(tracked)))
}

/// Transparent provider-owned Responses stream. It rewrites only response IDs
/// and deliberately performs no terminal accounting: the provider may keep
/// running after the downstream body is dropped, and the caller may replay a
/// terminal event through `starting_after`.
struct ResponseLocatorSseStream {
    inner: SseByteStream,
    decoder: SseDecoder,
    locator: Option<ResponseLocatorRewrite>,
    terminated: bool,
    _quota_permit: Option<QuotaPermit>,
}

impl ResponseLocatorSseStream {
    fn new(
        inner: SseByteStream,
        locator: Option<ResponseLocatorRewrite>,
        quota_permit: Option<QuotaPermit>,
    ) -> Self {
        Self {
            inner,
            decoder: SseDecoder::default(),
            locator,
            terminated: false,
            _quota_permit: quota_permit,
        }
    }
}

impl Stream for ResponseLocatorSseStream {
    type Item = Result<Bytes, BackendError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        if this.terminated {
            return Poll::Ready(None);
        }
        if this.locator.is_none() {
            return this.inner.as_mut().poll_next(cx);
        }
        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    let frames = this.decoder.push(&bytes);
                    if frames.is_empty() {
                        continue;
                    }
                    let locator = this.locator.as_ref().expect("checked above");
                    return Poll::Ready(Some(Ok(rewrite_locator_frames(
                        &frames,
                        &locator.codec,
                        &locator.connection_id,
                        locator.model_route_id.as_deref(),
                    ))));
                }
                Poll::Ready(Some(Err(error))) => {
                    this.terminated = true;
                    return Poll::Ready(Some(Err(error)));
                }
                Poll::Ready(None) => {
                    let frames = this.decoder.finish();
                    this.terminated = true;
                    if frames.is_empty() {
                        return Poll::Ready(None);
                    }
                    let locator = this.locator.as_ref().expect("checked above");
                    return Poll::Ready(Some(Ok(rewrite_locator_frames(
                        &frames,
                        &locator.codec,
                        &locator.connection_id,
                        locator.model_route_id.as_deref(),
                    ))));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

fn native_streaming_is_supported(route: &crate::backend::ResolvedRoute) -> bool {
    route
        .capabilities
        .profile
        .operation(Protocol::OpenaiResponses, ProtocolOperation::Create)
        .and_then(|operation| operation.features.get(&EndpointFeature::Streaming))
        != Some(&Support::Unsupported)
}

async fn create_non_streaming(
    span: LlmSpanWriter,
    state: crate::AppState,
    api_key: ResolvedApiKey,
    mut request: ResponsesRequest,
    original_model: String,
    request_headers: ResponsesRequestHeaders,
) -> Response {
    let background = request.background();
    let request_value = serde_json::to_value(&request).ok();
    // Hosted tools leave admission a soft cap over the visible input, and leave
    // no input a missing-usage estimate could trust.
    let admission_tokens = request_value
        .as_ref()
        .and_then(NormalizedUsageInput::visible_responses_json)
        .map(|input| input.conservative_tokens());
    let normalized_usage_input = request_value
        .as_ref()
        .and_then(NormalizedUsageInput::from_responses_json);
    let route = match resolve_authorized_primary(&span, &state, &api_key, &original_model).await {
        Ok(route) => route,
        Err(response) => return response,
    };
    let selection = match crate::judge::routing::select(
        &state,
        &api_key,
        &original_model,
        &Default::default(),
        route,
        || crate::judge::input::JudgeInput::responses(&request),
        &span,
    )
    .await
    {
        Ok(selection) => selection,
        Err(error) => return crate::handlers::backend_error_response(&error),
    };
    let selected_model = selection.plan.primary_model.clone();
    let background_event = if background {
        match state
            .background_responses
            .begin(&state, &api_key, &original_model, &span)
            .await
        {
            Ok(event) => Some(event),
            Err(error) => return crate::handlers::backend_error_response(&error),
        }
    } else {
        None
    };
    request.set_background_accounting_id(background_event);
    let accounting = state.background_responses.clone();
    let headers_for_dispatch = request_headers.clone();
    let locator_for_eligibility = state.response_locator.clone();
    let locator_for_dispatch = state.response_locator.clone();
    let requested_route_id = selection.plan.requested_route_id.clone();
    let route_id_for_dispatch = requested_route_id.clone();
    let result = try_with_fallbacks_if(
        &span,
        &state,
        request,
        selection.plan,
        &api_key,
        admission_tokens,
        move |route, request| {
            responses_request_is_eligible(route, request)
                .and_then(|_| locator_matches_route(&locator_for_eligibility, route, request))
        },
        move |route, mut request| {
            let backend = route.backend.clone();
            let mode = responses_create_mode(route);
            let connection_id = route.resolution.connection_id.clone();
            let model_route_id = route_id_for_dispatch.clone();
            let locator = locator_for_dispatch.clone();
            let prepared = if matches!(mode, DeliveryMode::Native | DeliveryMode::Adapted) {
                prepare_native_response_request(&locator, route, &mut request)
            } else {
                Ok(())
            };
            let headers = headers_for_dispatch.clone();
            let accounting = accounting.clone();
            let resolution = route.resolution.clone();
            async move {
                prepared?;
                if let Some(event) = background_event {
                    accounting
                        .prepare(event, &resolution, &resolution.upstream_model_id)
                        .await?;
                }
                match mode {
                    DeliveryMode::Native | DeliveryMode::Adapted => {
                        let mut response = backend.create_response(request, &headers).await?;
                        if !background {
                            wrap_response_id(
                                &locator,
                                connection_id.as_deref(),
                                model_route_id.as_deref(),
                                &mut response,
                            )?;
                        }
                        Ok(response)
                    }
                    DeliveryMode::Translated | DeliveryMode::Unknown => {
                        create_response_via_chat(backend.as_ref(), request).await
                    }
                    DeliveryMode::Unsupported => Err(BackendError::api_status(
                        400,
                        "Responses create is unsupported for this route",
                    )),
                }
            }
        },
    )
    .await;

    let (mut upstream, fallback_model, resolution, rate_limit_permit) = match result {
        Ok(result) => result,
        Err(error) => {
            if let Some(event) = background_event {
                if super::routing::definite_background_rejection(&error) {
                    state
                        .background_responses
                        .reject_submission(&state, event)
                        .await;
                } else {
                    state.background_responses.submission_failed(event).await;
                }
            }
            return crate::handlers::backend_error_response(&error);
        }
    };
    crate::judge::routing::spawn_shadow_if_enabled(
        state.clone(),
        api_key.clone(),
        selection.shadow,
        span.clone(),
    );
    set_route_span_attributes(&span, &resolution);
    let served_model = fallback_model.as_deref().unwrap_or(&selected_model);
    set_served_model_attributes(&span, &original_model, served_model);

    if let Some(event) = background_event {
        let result = async {
            state
                .background_responses
                .prepare(event, &resolution, served_model)
                .await?;
            state
                .background_responses
                .accept(event, &upstream.body)
                .await?;
            state.background_responses.settle(&state, event).await
        }
        .await;
        if let Err(error) = result {
            return crate::handlers::backend_error_response(&error);
        }
        if let Err(error) = wrap_response_id(
            &state.response_locator,
            resolution.connection_id.as_deref(),
            requested_route_id.as_deref(),
            &mut upstream,
        ) {
            return crate::handlers::backend_error_response(&error);
        }
    }

    if !background {
        let reported = upstream
            .body
            .get("usage")
            .and_then(TokenUsage::from_responses_value);
        let estimated = || {
            let mut estimate = UsageEstimateState::new(&resolution, normalized_usage_input)?;
            estimate.observe_json(&upstream.body);
            Some(TokenUsage::from_estimate(estimate))
        };
        let Some(token_usage) = reported.or_else(estimated) else {
            return super::usage_write_error_response();
        };
        token_usage.stamp(&span);
        let write = usage::record_future(
            state.clone(),
            UsageAttribution::new(&api_key, &resolution),
            token_usage,
        );
        if usage::with_quota_settlement(write, rate_limit_permit, Some(token_usage.total_tokens))
            .await
            .is_err()
        {
            return super::usage_write_error_response();
        }
    }
    set_output_span_attributes(&span, &upstream.body);
    span.apply_outcome_sampling(&state.sampling_gate, &upstream.body);

    let mut response = crate::handlers::native_body_response(upstream.response);
    stamp_routing_headers(
        response.headers_mut(),
        &original_model,
        served_model,
        fallback_model.as_deref(),
    );
    response
}

async fn create_streaming(
    span: LlmSpanWriter,
    state: crate::AppState,
    api_key: ResolvedApiKey,
    mut request: ResponsesRequest,
    original_model: String,
    request_headers: ResponsesRequestHeaders,
    start: Instant,
) -> Response {
    let background = request.background();
    let request_value = serde_json::to_value(&request).ok();
    // Hosted tools leave admission a soft cap over the visible input, and leave
    // no input a missing-usage estimate could trust.
    let admission_tokens = request_value
        .as_ref()
        .and_then(NormalizedUsageInput::visible_responses_json)
        .map(|input| input.conservative_tokens());
    let normalized_usage_input = request_value
        .as_ref()
        .and_then(NormalizedUsageInput::from_responses_json);
    let route = match resolve_authorized_primary(&span, &state, &api_key, &original_model).await {
        Ok(route) => route,
        Err(response) => return response,
    };
    let selection = match crate::judge::routing::select(
        &state,
        &api_key,
        &original_model,
        &Default::default(),
        route,
        || crate::judge::input::JudgeInput::responses(&request),
        &span,
    )
    .await
    {
        Ok(selection) => selection,
        Err(error) => return crate::handlers::backend_error_response(&error),
    };
    let selected_model = selection.plan.primary_model.clone();
    let background_event = if background {
        match state
            .background_responses
            .begin(&state, &api_key, &original_model, &span)
            .await
        {
            Ok(event) => Some(event),
            Err(error) => return crate::handlers::backend_error_response(&error),
        }
    } else {
        None
    };
    request.set_background_accounting_id(background_event);
    let accounting = state.background_responses.clone();
    let headers_for_dispatch = request_headers.clone();
    let locator_for_eligibility = state.response_locator.clone();
    let locator_for_dispatch = state.response_locator.clone();
    let requested_route_id = selection.plan.requested_route_id.clone();
    let result = try_with_fallbacks_if(
        &span,
        &state,
        request,
        selection.plan,
        &api_key,
        admission_tokens,
        move |route, request| {
            responses_request_is_eligible(route, request)
                .and_then(|_| locator_matches_route(&locator_for_eligibility, route, request))
        },
        move |route, mut request| {
            let backend = route.backend.clone();
            let mode = responses_create_mode(route);
            let native_streaming = native_streaming_is_supported(route);
            let locator = locator_for_dispatch.clone();
            let prepared = if matches!(mode, DeliveryMode::Native | DeliveryMode::Adapted) {
                prepare_native_response_request(&locator, route, &mut request)
            } else {
                Ok(())
            };
            let headers = headers_for_dispatch.clone();
            let accounting = accounting.clone();
            let resolution = route.resolution.clone();
            async move {
                prepared?;
                if let Some(event) = background_event {
                    accounting
                        .prepare(event, &resolution, &resolution.upstream_model_id)
                        .await?;
                }
                match mode {
                    DeliveryMode::Native | DeliveryMode::Adapted if native_streaming => {
                        open_stream_with_deadline(backend.create_response_stream(request, &headers))
                            .await
                    }
                    DeliveryMode::Native
                    | DeliveryMode::Adapted
                    | DeliveryMode::Translated
                    | DeliveryMode::Unknown => {
                        compat_response_with_deadline(create_response_stream_via_chat(
                            backend.as_ref(),
                            request,
                        ))
                        .await
                    }
                    DeliveryMode::Unsupported => Err(BackendError::api_status(
                        400,
                        "Responses create is unsupported for this route",
                    )),
                }
            }
        },
    )
    .await;

    let (upstream, fallback_model, resolution, rate_limit_permit) = match result {
        Ok(result) => result,
        Err(error) => {
            if let Some(event) = background_event {
                if super::routing::definite_background_rejection(&error) {
                    state
                        .background_responses
                        .reject_submission(&state, event)
                        .await;
                } else {
                    state.background_responses.submission_failed(event).await;
                }
            }
            return crate::handlers::backend_error_response(&error);
        }
    };
    crate::judge::routing::spawn_shadow_if_enabled(
        state.clone(),
        api_key.clone(),
        selection.shadow, // if shadow mode not set, this is None, and function early exits
        span.clone(),
    );
    set_route_span_attributes(&span, &resolution);
    let served_model = fallback_model.as_deref().unwrap_or(&selected_model);
    set_served_model_attributes(&span, &original_model, served_model);

    let BackendSseResponse {
        stream,
        headers: upstream_headers,
        status: upstream_status,
        content_type: upstream_content_type,
    } = upstream;
    let locator = resolution
        .connection_id
        .clone()
        .map(|connection_id| ResponseLocatorRewrite {
            codec: state.response_locator.clone(),
            connection_id,
            model_route_id: requested_route_id,
        });
    let stream: SseByteStream = if background {
        let event = background_event.expect("background submission has accounting identity");
        if let Err(error) = state
            .background_responses
            .prepare(event, &resolution, served_model)
            .await
        {
            return crate::handlers::backend_error_response(&error);
        }
        match track_background_stream(
            state.clone(),
            resolution
                .connection_id
                .clone()
                .expect("background connection"),
            Some(event),
            stream,
        )
        .await
        {
            Ok(stream) => Box::pin(ResponseLocatorSseStream::new(stream, locator, None)),
            Err(error) => {
                state.background_responses.submission_failed(event).await;
                return crate::handlers::backend_error_response(&error);
            }
        }
    } else {
        Box::pin(ResponsesSseTap::new(
            stream,
            span,
            state.clone(),
            UsageAttribution::new(&api_key, &resolution),
            start,
            normalized_usage_input,
            rate_limit_permit,
            locator,
        ))
    };
    let mut response = Response::builder()
        .status(upstream_status)
        .header(header::CONTENT_TYPE, upstream_content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .expect("valid Responses SSE headers");
    crate::handlers::apply_forwarded_headers(response.headers_mut(), &upstream_headers);
    crate::handlers::stamp_trace_headers(response.headers_mut());
    stamp_routing_headers(
        response.headers_mut(),
        &original_model,
        served_model,
        fallback_model.as_deref(),
    );
    response
}

/// Native remote compaction. Compaction is intentionally not translated or
/// retried onto chat-only fallbacks because its opaque item has no equivalent
/// there.
pub async fn compact_response(
    State(state): State<crate::AppState>,
    api_key: ResolvedApiKey,
    headers: HeaderMap,
    Json(mut request): Json<crate::schema_json::SchemaJson>,
) -> Response {
    let normalized_usage_input = NormalizedUsageInput::from_responses_json(&request);
    // Hosted tools, stored history, and opaque content such as encrypted
    // reasoning leave admission a soft cap over the visible input.
    let admission_input = NormalizedUsageInput::visible_responses_json(&request);
    let soft_cap = ToolContext::of_responses(&request) == ToolContext::Hosted
        || crate::usage_estimator::provider_adds_input(&request);
    let span = tracing::info_span!("responses_compact", otel.name = "responses.compact",);
    let span = LlmSpanWriter::new(span, state.object_store.clone(), LlmSpanKind::Llm);
    super::stamp_authenticated_principal(&span, &api_key);
    span.clone()
        .instrument(async move {
            let current_span = span;
            set_input_value_span_attributes(&current_span, &request);
            current_span.set_stream(false);
            let Some(original_model) = request
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned)
            else {
                return crate::handlers::backend_error_response(&BackendError::api_status(
                    400,
                    "Responses compaction requires a string `model` field",
                ));
            };
            let route =
                match resolve_authorized_primary(&current_span, &state, &api_key, &original_model)
                    .await
                {
                    Ok(route) => route,
                    Err(response) => return response,
                };
            let compact_mode = route
                .capabilities
                .profile
                .operation(Protocol::OpenaiResponses, ProtocolOperation::Compact)
                .map_or(DeliveryMode::Unknown, |operation| operation.mode);
            if !matches!(compact_mode, DeliveryMode::Native | DeliveryMode::Adapted) {
                return crate::handlers::backend_error_response(&BackendError::api_status(
                    400,
                    "Responses compaction is unsupported for this route",
                ));
            }
            let input_tokens = admission_input.map(|input| input.conservative_tokens());
            let output_tokens = request
                .get("max_output_tokens")
                .and_then(Value::as_u64)
                .or(route.capabilities.profile.limits.max_output_tokens);
            let token_reservation = input_tokens
                .zip(output_tokens)
                .map(|(input, output)| input.saturating_add(output));
            if let Err(error) =
                request.set_member("model", Value::String(route.stripped_model.clone()).into())
            {
                return crate::handlers::backend_error_response(&BackendError::Other(format!(
                    "failed to update validated Responses compaction request: {error}"
                )));
            }
            let mut quota_permit = match acquire_single_route_quota(
                &state,
                &api_key,
                &route.resolution,
                RateLimitModel::Exact(&original_model),
                TokenReservation::from_option(token_reservation).with_soft_cap(soft_cap),
                input_tokens,
            )
            .await
            {
                Ok(permit) => permit,
                Err(error) => return crate::handlers::backend_error_response(&error),
            };
            let forwarded = ResponsesRequestHeaders::from_header_map(&headers);
            let upstream = match route.backend.compact_response(request, &forwarded).await {
                Ok(upstream) => upstream,
                Err(error) => {
                    if let Some(permit) = quota_permit.take() {
                        permit.finish_failed().await;
                    }
                    return crate::handlers::backend_error_response(&error);
                }
            };

            set_output_span_attributes(&current_span, &upstream.body);
            let reported = upstream
                .body
                .get("usage")
                .and_then(TokenUsage::from_responses_value);
            let estimated = || {
                let mut estimate =
                    UsageEstimateState::new(&route.resolution, normalized_usage_input)?;
                estimate.observe_json(&upstream.body);
                Some(TokenUsage::from_estimate(estimate))
            };
            let Some(token_usage) = reported.or_else(estimated) else {
                return super::usage_write_error_response();
            };
            token_usage.stamp(&current_span);
            let write = usage::record_future(
                state.clone(),
                UsageAttribution::new(&api_key, &route.resolution),
                token_usage,
            );
            if usage::with_quota_settlement(
                write,
                quota_permit.take(),
                Some(token_usage.total_tokens),
            )
            .await
            .is_err()
            {
                return super::usage_write_error_response();
            }
            crate::handlers::native_body_response(upstream.response)
        })
        .await
}

#[allow(clippy::result_large_err)]
async fn resolve_stored_response(
    state: &crate::AppState,
    api_key: &ResolvedApiKey,
    encoded_id: &str,
    operation: ProtocolOperation,
) -> Result<
    (
        crate::backend::ResolvedRoute,
        crate::response_locator::ResponseLocator,
    ),
    Response,
> {
    let locator = state.response_locator.decode(encoded_id).map_err(|error| {
        crate::handlers::backend_error_response(&BackendError::api_status(400, error.to_string()))
    })?;
    let route = state
        .backend
        .resolve_connection(&locator.connection_id)
        .await
        .map_err(|error| crate::handlers::backend_error_response(&error))?;
    api_key
        .0
        .permission
        .allows_egress(
            locator.model_route_id.as_deref(),
            &route.resolution.provider_kind,
            route.resolution.connection_id.as_deref(),
        )
        .map_err(|denied| crate::handlers::forbidden_response(denied.to_string()))?;
    let mode = route
        .capabilities
        .profile
        .operation(Protocol::OpenaiResponses, operation)
        .map_or(DeliveryMode::Unknown, |profile| profile.mode);
    if !matches!(mode, DeliveryMode::Native | DeliveryMode::Adapted) {
        return Err(crate::handlers::backend_error_response(
            &BackendError::api_status(
                400,
                format!("Responses {operation:?} is unsupported for this connection"),
            ),
        ));
    }
    Ok((route, locator))
}

pub async fn retrieve_response(
    State(state): State<crate::AppState>,
    api_key: ResolvedApiKey,
    headers: HeaderMap,
    Path(response_id): Path<String>,
    Query(query): Query<RetrieveResponseQuery>,
) -> Response {
    if query.starting_after.is_some() && !query.stream.unwrap_or(false) {
        return crate::handlers::invalid_request_response(
            "starting_after requires stream=true when retrieving a Response",
        );
    }
    let (route, locator) =
        match resolve_stored_response(&state, &api_key, &response_id, ProtocolOperation::Retrieve)
            .await
        {
            Ok(resolved) => resolved,
            Err(response) => return response,
        };
    if query.stream.unwrap_or(false)
        && route
            .capabilities
            .profile
            .operation(Protocol::OpenaiResponses, ProtocolOperation::Retrieve)
            .and_then(|operation| operation.features.get(&EndpointFeature::Streaming))
            == Some(&Support::Unsupported)
    {
        return crate::handlers::backend_error_response(&BackendError::api_status(
            400,
            "Responses stream resumption is unsupported for this connection",
        ));
    }
    let mut quota_permit =
        match super::routing::acquire_response_lifecycle_quota(&state, &api_key, &route.resolution)
            .await
        {
            Ok(permit) => permit,
            Err(error) => return crate::handlers::backend_error_response(&error),
        };
    let forwarded = ResponsesRequestHeaders::from_header_map(&headers);
    if query.stream.unwrap_or(false) {
        return match route
            .backend
            .retrieve_response_stream(
                &locator.upstream_response_id,
                query.starting_after,
                &forwarded,
            )
            .await
        {
            Ok(upstream) => {
                let BackendSseResponse {
                    stream,
                    headers: upstream_headers,
                    status: upstream_status,
                    content_type: upstream_content_type,
                } = upstream;
                let locator = route.resolution.connection_id.clone().map(|connection_id| {
                    ResponseLocatorRewrite {
                        codec: state.response_locator.clone(),
                        connection_id,
                        model_route_id: locator.model_route_id.clone(),
                    }
                });
                let stream = match track_background_stream(
                    state.clone(),
                    route.resolution.connection_id.clone().unwrap_or_default(),
                    None,
                    stream,
                )
                .await
                {
                    Ok(stream) => stream,
                    Err(error) => return crate::handlers::backend_error_response(&error),
                };
                let stream = ResponseLocatorSseStream::new(stream, locator, quota_permit.take());
                let mut response = Response::builder()
                    .status(upstream_status)
                    .header(header::CONTENT_TYPE, upstream_content_type)
                    .header(header::CACHE_CONTROL, "no-cache")
                    .header(header::CONNECTION, "keep-alive")
                    .body(Body::from_stream(stream))
                    .expect("valid Responses SSE headers");
                crate::handlers::apply_forwarded_headers(response.headers_mut(), &upstream_headers);
                crate::handlers::stamp_trace_headers(response.headers_mut());
                response
            }
            Err(error) => {
                if let Some(permit) = quota_permit.take() {
                    permit.finish_failed().await;
                }
                crate::handlers::backend_error_response(&error)
            }
        };
    }
    match route
        .backend
        .retrieve_response(&locator.upstream_response_id, &forwarded)
        .await
    {
        Ok(mut upstream) => {
            if let Some(permit) = quota_permit.take() {
                permit.finish(None).await;
            }
            if let Err(error) = state
                .background_responses
                .observe(&state, &locator.connection_id, &upstream.body)
                .await
            {
                return crate::handlers::backend_error_response(&error);
            }
            if let Err(error) = wrap_response_id(
                &state.response_locator,
                route.resolution.connection_id.as_deref(),
                locator.model_route_id.as_deref(),
                &mut upstream,
            ) {
                return crate::handlers::backend_error_response(&error);
            }
            crate::handlers::native_body_response(upstream.response)
        }
        Err(error) => {
            if let Some(permit) = quota_permit.take() {
                permit.finish_failed().await;
            }
            crate::handlers::backend_error_response(&error)
        }
    }
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct RetrieveResponseQuery {
    pub stream: Option<bool>,
    pub starting_after: Option<u64>,
}

pub async fn cancel_response(
    State(state): State<crate::AppState>,
    api_key: ResolvedApiKey,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Response {
    let (route, locator) =
        match resolve_stored_response(&state, &api_key, &response_id, ProtocolOperation::Cancel)
            .await
        {
            Ok(resolved) => resolved,
            Err(response) => return response,
        };
    let mut quota_permit =
        match super::routing::acquire_response_lifecycle_quota(&state, &api_key, &route.resolution)
            .await
        {
            Ok(permit) => permit,
            Err(error) => return crate::handlers::backend_error_response(&error),
        };
    let forwarded = ResponsesRequestHeaders::from_header_map(&headers);
    match route
        .backend
        .cancel_response(&locator.upstream_response_id, &forwarded)
        .await
    {
        Ok(mut upstream) => {
            if let Some(permit) = quota_permit.take() {
                permit.finish(None).await;
            }
            if let Err(error) = state
                .background_responses
                .observe(&state, &locator.connection_id, &upstream.body)
                .await
            {
                return crate::handlers::backend_error_response(&error);
            }
            if let Err(error) = wrap_response_id(
                &state.response_locator,
                route.resolution.connection_id.as_deref(),
                locator.model_route_id.as_deref(),
                &mut upstream,
            ) {
                return crate::handlers::backend_error_response(&error);
            }
            crate::handlers::native_body_response(upstream.response)
        }
        Err(error) => {
            if let Some(permit) = quota_permit.take() {
                permit.finish_failed().await;
            }
            crate::handlers::backend_error_response(&error)
        }
    }
}

pub async fn delete_response(
    State(state): State<crate::AppState>,
    api_key: ResolvedApiKey,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Response {
    let (route, locator) =
        match resolve_stored_response(&state, &api_key, &response_id, ProtocolOperation::Delete)
            .await
        {
            Ok(resolved) => resolved,
            Err(response) => return response,
        };
    let mut quota_permit =
        match super::routing::acquire_response_lifecycle_quota(&state, &api_key, &route.resolution)
            .await
        {
            Ok(permit) => permit,
            Err(error) => return crate::handlers::backend_error_response(&error),
        };
    let forwarded = ResponsesRequestHeaders::from_header_map(&headers);
    match route
        .backend
        .delete_response(&locator.upstream_response_id, &forwarded)
        .await
    {
        Ok(mut upstream) => {
            if let Some(permit) = quota_permit.take() {
                permit.finish(None).await;
            }
            if let Err(error) = wrap_response_id(
                &state.response_locator,
                route.resolution.connection_id.as_deref(),
                locator.model_route_id.as_deref(),
                &mut upstream,
            ) {
                return crate::handlers::backend_error_response(&error);
            }
            crate::handlers::native_body_response(upstream.response)
        }
        Err(error) => {
            if let Some(permit) = quota_permit.take() {
                permit.finish_failed().await;
            }
            crate::handlers::backend_error_response(&error)
        }
    }
}

fn set_served_model_attributes(span: &LlmSpanWriter, requested: &str, served: &str) {
    span.set_requested_served_models(requested, served);
}

#[derive(Debug)]
struct TraceToolCall {
    id: String,
    name: String,
    arguments: Option<String>,
}

#[derive(Debug)]
struct TraceMessage {
    source_index: usize,
    role: String,
    content: Option<String>,
    tool_call_id: Option<String>,
    tool_calls: Vec<TraceToolCall>,
}

fn trace_content(value: &Value, log_content: bool) -> Option<String> {
    if !log_content {
        return None;
    }
    match value {
        Value::Null => None,
        Value::String(text) => (!text.is_empty()).then(|| text.clone()),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter_map(|part| match part {
                    Value::String(text) => Some(text.clone()),
                    Value::Object(object) => object
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| {
                            object
                                .get("refusal")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        })
                        .or_else(|| {
                            object
                                .get("output")
                                .and_then(|output| trace_content(output, true))
                        }),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        // Tool results may be structured JSON. Content logging authorizes the
        // value, but encrypted reasoning is still removed recursively.
        value => serde_json::to_string(&redact_if_needed(value)).ok(),
    }
}

fn trace_tool_call(item: &serde_json::Map<String, Value>, log_content: bool) -> TraceToolCall {
    TraceToolCall {
        id: item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        name: item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                item.get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("tool_call")
            })
            .to_string(),
        arguments: item
            .get("arguments")
            .or_else(|| item.get("input"))
            .or_else(|| item.get("action"))
            .and_then(|arguments| trace_content(arguments, log_content)),
    }
}

fn trace_messages(
    value: &Value,
    log_content: bool,
    include_instructions: bool,
) -> Vec<TraceMessage> {
    let mut messages = Vec::new();
    let object = value.as_object();
    if include_instructions {
        if let Some(instructions) = object
            .and_then(|object| object.get("instructions"))
            .filter(|instructions| !instructions.is_null())
        {
            messages.push(TraceMessage {
                source_index: 0,
                role: "system".to_string(),
                content: trace_content(instructions, log_content),
                tool_call_id: None,
                tool_calls: Vec::new(),
            });
        }
    }
    let source_offset = messages.len();
    let items = object
        .and_then(|object| {
            object.get(if include_instructions {
                "input"
            } else {
                "output"
            })
        })
        .unwrap_or(value);
    if let Value::String(text) = items {
        messages.push(TraceMessage {
            source_index: source_offset,
            role: "user".to_string(),
            content: (log_content && !text.is_empty()).then(|| text.clone()),
            tool_call_id: None,
            tool_calls: Vec::new(),
        });
        return messages;
    }
    let Some(items) = items.as_array() else {
        return messages;
    };
    for (item_index, item) in items.iter().enumerate() {
        let Some(item) = item.as_object() else {
            continue;
        };
        let source_index = source_offset + item_index;
        match item.get("type").and_then(Value::as_str) {
            Some("message") | None if item.contains_key("role") => {
                let Some(role) = item.get("role").and_then(Value::as_str) else {
                    continue;
                };
                messages.push(TraceMessage {
                    source_index,
                    role: role.to_string(),
                    content: item
                        .get("content")
                        .and_then(|content| trace_content(content, log_content)),
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                });
            }
            Some(kind) if kind.ends_with("_call") => {
                messages.push(TraceMessage {
                    source_index,
                    role: "assistant".to_string(),
                    content: None,
                    tool_call_id: None,
                    tool_calls: vec![trace_tool_call(item, log_content)],
                });
            }
            Some(kind) if kind.ends_with("_call_output") => {
                messages.push(TraceMessage {
                    source_index,
                    role: "tool".to_string(),
                    content: item
                        .get("output")
                        .and_then(|output| trace_content(output, log_content)),
                    tool_call_id: item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    tool_calls: Vec::new(),
                });
            }
            _ => {}
        }
    }
    messages
}

fn stamp_trace_messages(
    span: &LlmSpanWriter,
    prefix: &str,
    messages: &[TraceMessage],
    direction: ContentDirection,
) {
    struct PlannedMessage {
        source_index: usize,
        fields: Vec<super::trace_attributes::ReservedLlmAttributes>,
    }

    let mut planned = Vec::new();
    let mut recorded = 0usize;
    let mut omitted_metadata = 0usize;
    let mut omitted_content = 0usize;

    // Select newest-to-oldest, then restore chronological order and dense
    // OpenInference indices for the retained subset below.
    let order = (0..messages.len()).rev();
    for index in order {
        let message = &messages[index];
        let Some(role) = span.reserve_metadata_group(
            direction,
            [("message.role".to_string(), message.role.clone())],
        ) else {
            omitted_metadata += 1;
            continue;
        };
        let mut fields = vec![role];
        recorded += 1;

        if let Some(tool_call_id) = &message.tool_call_id {
            if let Some(reservation) = span.reserve_metadata_group(
                direction,
                [("message.tool_call_id".to_string(), tool_call_id.clone())],
            ) {
                fields.push(reservation);
            } else {
                omitted_metadata += 1;
            }
        }
        for (tool_index, tool_call) in message.tool_calls.iter().enumerate() {
            if let Some(reservation) = span.reserve_metadata_group(
                direction,
                [
                    (
                        format!("message.tool_calls.{tool_index}.tool_call.id"),
                        tool_call.id.clone(),
                    ),
                    (
                        format!("message.tool_calls.{tool_index}.tool_call.function.name"),
                        tool_call.name.clone(),
                    ),
                ],
            ) {
                fields.push(reservation);
                if let Some(arguments) = &tool_call.arguments {
                    if let Some(reservation) = span.reserve_content(
                        direction,
                        format!("message.tool_calls.{tool_index}.tool_call.function.arguments"),
                        arguments.clone(),
                    ) {
                        fields.push(reservation);
                    } else {
                        omitted_content += 1;
                    }
                }
            } else {
                omitted_metadata += 2;
                omitted_content += usize::from(tool_call.arguments.is_some());
            }
        }
        if let Some(content) = &message.content {
            if let Some(reservation) =
                span.reserve_content(direction, "message.content".to_string(), content.clone())
            {
                fields.push(reservation);
            } else {
                omitted_content += 1;
            }
        }
        planned.push(PlannedMessage {
            source_index: message.source_index,
            fields,
        });
    }
    // Export the newest retained suffix chronologically with dense indices.
    planned.sort_by_key(|message| message.source_index);
    for (message_index, message) in planned.into_iter().enumerate() {
        let field_prefix = format!("{prefix}.{message_index}.");
        for reservation in message.fields {
            span.write_reserved(&field_prefix, reservation);
        }
    }
    if recorded < messages.len() || omitted_metadata > 0 || omitted_content > 0 {
        span.truncation(
            direction,
            "messages",
            serde_json::json!({
                "total": messages.len(),
                "recorded": recorded,
                "omitted": messages.len().saturating_sub(recorded),
                "metadata_fields_omitted": omitted_metadata,
                "content_fields_omitted": omitted_content,
            }),
        );
    }
}

fn set_input_span_attributes(span: &LlmSpanWriter, request: &ResponsesRequest) {
    set_input_value_span_attributes(span, &request.retained());
}

fn set_input_value_span_attributes(
    span: &LlmSpanWriter,
    retained: &crate::schema_json::SchemaJson,
) {
    let value = retained.value();
    if let Some(object) = value.as_object() {
        // Invocation parameters are exported even in metadata-only mode, so
        // keep this list deliberately limited to scalar generation controls.
        // Tools, schemas, metadata, cache keys, and forward-compatible fields
        // may contain customer content and belong only in the content-gated
        // input payload below.
        const SAFE_PARAMETERS: &[&str] = &[
            "background",
            "max_output_tokens",
            "parallel_tool_calls",
            "service_tier",
            "store",
            "stream",
            "temperature",
            "top_p",
            "truncation",
        ];
        let parameters = object
            .iter()
            .filter(|(key, value)| {
                SAFE_PARAMETERS.contains(&key.as_str())
                    && matches!(
                        value,
                        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
                    )
            })
            .filter_map(|(key, _)| {
                retained
                    .member(key)
                    .ok()
                    .flatten()
                    .map(|value| (key.clone(), value))
            })
            .collect::<Vec<_>>();
        let parameters = crate::schema_json::SchemaJson::object(parameters);
        if let Ok(parameters) = serde_json::to_string(&parameters) {
            if parameters.len() <= MAX_INVOCATION_PARAMETERS_BYTES {
                span.set_invocation_parameters_json(parameters);
            }
        }
        let log_content = span.content_enabled();
        if log_content {
            let content = ["instructions", "input", "tools"]
                .into_iter()
                .filter_map(|key| {
                    retained.member(key).ok().flatten().map(|value| {
                        value
                            .redact_encrypted_content()
                            .map(|value| (key.to_string(), value))
                    })
                })
                .collect::<serde_json::Result<Vec<_>>>();
            if let Ok(content) = content {
                span.record_json(
                    ContentDirection::Input,
                    &crate::schema_json::SchemaJson::object(content),
                );
            }
        }
        let messages = trace_messages(value, log_content, true);
        stamp_trace_messages(
            span,
            "llm.input_messages",
            &messages,
            ContentDirection::Input,
        );
    }
}

fn set_output_span_attributes(span: &LlmSpanWriter, response: &Value) {
    if let Some(status) = response.get("status").and_then(Value::as_str) {
        span.set_finish_reason(status);
    }
    let log_content = super::content_logging_enabled();
    let sanitized = log_content.then(|| redact_if_needed(response));
    if let Some(sanitized) = sanitized.as_ref() {
        span.record_json(ContentDirection::Output, sanitized.as_ref());
    }
    // Reuse the same sanitized tree (when one was built) instead of walking
    // `response` a second time and letting `trace_content` redact overlapping
    // sub-values from the original, unsanitized copy all over again.
    let source = sanitized.as_deref().unwrap_or(response);
    let messages = trace_messages(source, log_content, false);
    stamp_trace_messages(
        span,
        "llm.output_messages",
        &messages,
        ContentDirection::Output,
    );
}

/// Encrypted reasoning is deliberately never copied to trace attributes, even
/// when content logging is enabled. Unknown Responses items remain intact on
/// the wire; this function is used only for telemetry copies.
fn redact_encrypted_content(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(key, _)| key.as_str() != "encrypted_content")
                .map(|(key, value)| (key.clone(), redact_encrypted_content(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(redact_encrypted_content).collect()),
        value => value.clone(),
    }
}

/// Cheap, clone-free scan for whether `encrypted_content` appears anywhere in
/// `value`. Most responses carry no reasoning items at all, so this lets
/// every redaction call site skip the (necessarily whole-tree-cloning)
/// [`redact_encrypted_content`] walk in the common case instead of paying for
/// a clone that turns out to remove nothing.
fn contains_encrypted_content(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object.contains_key("encrypted_content")
                || object.values().any(contains_encrypted_content)
        }
        Value::Array(values) => values.iter().any(contains_encrypted_content),
        _ => false,
    }
}

/// Redact only when there is something to redact, borrowing `value` unchanged
/// otherwise so callers that already know their source is clean (or that
/// find it to be) never pay for a redundant clone/walk of the same tree.
fn redact_if_needed(value: &Value) -> Cow<'_, Value> {
    if contains_encrypted_content(value) {
        Cow::Owned(redact_encrypted_content(value))
    } else {
        Cow::Borrowed(value)
    }
}

#[derive(Debug)]
struct TerminalEvent {
    status: String,
    response: Option<Value>,
    usage: Option<Value>,
}

fn terminal_event(event_type: &str, data: &Value) -> Option<TerminalEvent> {
    let status = match event_type {
        "response.completed" => "completed",
        "response.failed" => "failed",
        "response.incomplete" => "incomplete",
        _ => return None,
    };
    let response = data.get("response").cloned();
    let usage = response
        .as_ref()
        .and_then(|response| response.get("usage"))
        .cloned();
    Some(TerminalEvent {
        status: status.to_string(),
        response,
        usage,
    })
}

fn transport_error_event(error: &BackendError, sequence_number: u64) -> Bytes {
    let payload = serde_json::json!({
        "type": "error",
        "sequence_number": sequence_number,
        "code": "stream_transport_error",
        "message": error.to_string(),
        "param": null,
    });
    Bytes::from(format!("event: error\ndata: {payload}\n\n"))
}

fn accounting_error_event(sequence_number: u64) -> Bytes {
    let payload = serde_json::json!({
        "type": "error",
        "sequence_number": sequence_number,
        "code": "usage_accounting_unavailable",
        "message": "Usage accounting is temporarily unavailable",
        "param": null,
    });
    Bytes::from(format!("event: error\ndata: {payload}\n\n"))
}

fn is_content_bearing_delta(event_type: Option<&str>, data: Option<&Value>) -> bool {
    let Some(event_type) = event_type else {
        return false;
    };
    let relevant = matches!(
        event_type,
        "response.output_text.delta"
            | "response.refusal.delta"
            | "response.function_call_arguments.delta"
            | "response.custom_tool_call_input.delta"
    ) || (event_type.starts_with("response.reasoning")
        && event_type.ends_with(".delta"));
    if !relevant {
        return false;
    }
    data.and_then(|data| data.get("delta").or_else(|| data.get("text")))
        .is_some_and(|delta| match delta {
            Value::String(text) => !text.is_empty(),
            Value::Array(values) => !values.is_empty(),
            Value::Object(object) => !object.is_empty(),
            _ => false,
        })
}

fn mark_client_cancelled(span: &LlmSpanWriter) {
    span.set_finish_reason("cancelled");
    span.set_error("client_cancelled", None);
    span.set_error_status("client disconnected");
}

fn stamp_upstream_error_once(span: &LlmSpanWriter, data: Option<&Value>, seen: &mut bool) {
    if *seen {
        return;
    }
    *seen = true;
    let message = data
        .and_then(|data| data.get("error").or(Some(data)))
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str);
    span.set_error("responses_stream_error", message);
}

/// A protocol tap. Complete SSE frames are decoded from a single buffer that
/// serves both telemetry/accounting and, when the winning route carries a
/// `connection_id`, response-id rewriting — the same decode pass that used to
/// run twice (once for id rewriting, once for the tap) now runs once. When no
/// locator rewrite is needed, successful upstream chunks are still returned
/// without reserialization.
struct ResponsesSseTap {
    inner: SseByteStream,
    decoder: SseDecoder,
    span: LlmSpanWriter,
    state: crate::AppState,
    attribution: UsageAttribution,
    finalized: bool,
    terminated: bool,
    next_sequence_number: u64,
    start: Instant,
    first_token_seen: bool,
    upstream_error_seen: bool,
    estimate: Option<UsageEstimateState>,
    estimated_output_observed: bool,
    pending_write: Option<UsageWriteFuture>,
    rate_limit_permit: Option<QuotaPermit>,
    pending_terminal: Option<Bytes>,
    end_after_write: bool,
    locator: Option<ResponseLocatorRewrite>,
}

impl ResponsesSseTap {
    #[allow(clippy::too_many_arguments)]
    fn new(
        inner: SseByteStream,
        span: LlmSpanWriter,
        state: crate::AppState,
        attribution: UsageAttribution,
        start: Instant,
        normalized_usage_input: Option<NormalizedUsageInput>,
        rate_limit_permit: Option<QuotaPermit>,
        locator: Option<ResponseLocatorRewrite>,
    ) -> Self {
        let estimate = UsageEstimateState::new(attribution.resolution(), normalized_usage_input);
        Self {
            inner,
            decoder: SseDecoder::default(),
            span,
            state,
            attribution,
            finalized: false,
            terminated: false,
            next_sequence_number: 0,
            start,
            first_token_seen: false,
            upstream_error_seen: false,
            estimate,
            estimated_output_observed: false,
            pending_write: None,
            rate_limit_permit,
            pending_terminal: None,
            end_after_write: false,
            locator,
        }
    }

    fn observe(&mut self, frame: SseFrame) {
        let parsed = serde_json::from_str::<Value>(&frame.data).ok();
        if let Some(sequence) = parsed
            .as_ref()
            .and_then(|data| data.get("sequence_number"))
            .and_then(Value::as_u64)
        {
            self.next_sequence_number = self.next_sequence_number.max(sequence.saturating_add(1));
        }
        let event_type = frame.event.clone().or_else(|| {
            parsed
                .as_ref()
                .and_then(|data| data.get("type"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
        if !self.first_token_seen
            && is_content_bearing_delta(event_type.as_deref(), parsed.as_ref())
        {
            self.first_token_seen = true;
            self.span
                .set_time_to_first_token_ms(self.start.elapsed().as_millis() as i64);
        }
        if is_content_bearing_delta(event_type.as_deref(), parsed.as_ref()) {
            if let (Some(estimate), Some(delta)) = (
                self.estimate.as_mut(),
                parsed
                    .as_ref()
                    .and_then(|data| data.get("delta").or_else(|| data.get("text"))),
            ) {
                estimate.observe_json(delta);
                self.estimated_output_observed = true;
            }
        }
        if event_type.as_deref() == Some("error") {
            stamp_upstream_error_once(&self.span, parsed.as_ref(), &mut self.upstream_error_seen);
        }
        if let (Some(event_type), Some(parsed)) = (event_type.as_deref(), parsed.as_ref()) {
            if let Some(terminal) = terminal_event(event_type, parsed) {
                self.finalize(Some(terminal));
            }
        }
    }

    fn finalize(&mut self, terminal: Option<TerminalEvent>) {
        if self.finalized {
            return;
        }
        self.finalized = true;
        if let Some(terminal) = terminal {
            let completed = terminal.status == "completed";
            if terminal.status != "completed" {
                self.span.set_error(terminal.status.clone(), None);
                self.span.set_error_status(terminal.status.clone());
            }
            if let Some(response) = terminal.response.as_ref() {
                let has_response_status = response.get("status").and_then(Value::as_str).is_some();
                set_output_span_attributes(&self.span, response);
                self.span
                    .apply_outcome_sampling(&self.state.sampling_gate, response);
                if !has_response_status {
                    self.span.set_finish_reason(terminal.status.clone());
                }
                if !self.estimated_output_observed {
                    if let Some(estimate) = self.estimate.as_mut() {
                        estimate.observe_json(response);
                    }
                }
            } else {
                self.span.set_finish_reason(terminal.status.clone());
            }
            let reported = terminal
                .usage
                .as_ref()
                .and_then(TokenUsage::from_responses_value);
            let token_usage = reported.or_else(|| {
                (terminal.status == "completed")
                    .then(|| self.estimate.take().map(TokenUsage::from_estimate))
                    .flatten()
            });
            if let Some(usage) = token_usage {
                usage.stamp(&self.span);
                let write =
                    usage::record_future(self.state.clone(), self.attribution.clone(), usage);
                let permit = completed.then(|| self.rate_limit_permit.take()).flatten();
                self.pending_write = Some(usage::with_quota_settlement(
                    write,
                    permit,
                    Some(usage.total_tokens),
                ));
            } else if terminal.status == "completed" {
                self.pending_write = Some(usage::missing_usage_future());
            }
        } else if let Some(estimate) = self.estimate.take() {
            let usage = TokenUsage::from_estimate(estimate);
            usage.stamp(&self.span);
            self.pending_write = Some(usage::record_future(
                self.state.clone(),
                self.attribution.clone(),
                usage,
            ));
        } else {
            self.pending_write = Some(usage::missing_usage_future());
        }
    }

    fn poll_usage_write(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Option<Poll<Option<Result<Bytes, BackendError>>>> {
        let future = self.pending_write.as_mut()?;
        match future.as_mut().poll(cx) {
            Poll::Pending => Some(Poll::Pending),
            Poll::Ready(Ok(())) => {
                self.pending_write = None;
                if let Some(bytes) = self.pending_terminal.take() {
                    Some(Poll::Ready(Some(Ok(bytes))))
                } else if self.end_after_write {
                    self.terminated = true;
                    Some(Poll::Ready(None))
                } else {
                    None
                }
            }
            Poll::Ready(Err(error)) => {
                tracing::error!(error = %error, "Responses stream usage could not be persisted");
                self.pending_write = None;
                self.pending_terminal = None;
                self.terminated = true;
                self.span
                    .set_error("usage_accounting_unavailable", Some(&error.to_string()));
                Some(Poll::Ready(Some(Ok(accounting_error_event(
                    self.next_sequence_number,
                )))))
            }
        }
    }
}

impl Stream for ResponsesSseTap {
    type Item = Result<Bytes, BackendError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        let span = this.span.clone();
        let _entered = span.enter();
        if this.terminated {
            return Poll::Ready(None);
        }
        if let Some(result) = this.poll_usage_write(cx) {
            return result;
        }
        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    let frames = this.decoder.push(&bytes);
                    if frames.is_empty() {
                        if this.locator.is_some() {
                            // A locator rewrite requires complete frames, so a
                            // chunk that only advances a partial line carries
                            // nothing to rewrite yet; poll upstream again
                            // rather than forwarding raw, unrewritten bytes.
                            continue;
                        }
                        return Poll::Ready(Some(Ok(bytes)));
                    }
                    let rewritten = this.locator.as_ref().map(|locator| {
                        rewrite_locator_frames(
                            &frames,
                            &locator.codec,
                            &locator.connection_id,
                            locator.model_route_id.as_deref(),
                        )
                    });
                    for frame in frames {
                        this.observe(frame);
                    }
                    let to_send = rewritten.unwrap_or(bytes);
                    return if this.pending_write.is_some() {
                        this.pending_terminal = Some(to_send);
                        this.poll_usage_write(cx).unwrap_or(Poll::Pending)
                    } else {
                        Poll::Ready(Some(Ok(to_send)))
                    };
                }
                Poll::Ready(Some(Err(error))) => {
                    tracing::error!(%error, "Responses upstream stream failed after opening");
                    this.span.set_error("stream_transport_error", None);
                    this.span.set_finish_reason("failed");
                    this.span.set_error_status(error.to_string());
                    this.finalize(None);
                    if let Some(future) = this.pending_write.take() {
                        usage::detach_future(future);
                    }
                    this.terminated = true;
                    return Poll::Ready(Some(Ok(transport_error_event(
                        &error,
                        this.next_sequence_number,
                    ))));
                }
                Poll::Ready(None) => {
                    let finish_frames = this.decoder.finish();
                    let trailing = this.locator.as_ref().and_then(|locator| {
                        (!finish_frames.is_empty()).then(|| {
                            rewrite_locator_frames(
                                &finish_frames,
                                &locator.codec,
                                &locator.connection_id,
                                locator.model_route_id.as_deref(),
                            )
                        })
                    });
                    for frame in finish_frames {
                        this.observe(frame);
                    }
                    if !this.finalized {
                        this.span.set_error("stream_incomplete", None);
                        this.span.set_finish_reason("incomplete");
                        this.span
                            .set_error_status("stream closed before response.completed");
                        this.finalize(None);
                    }
                    if trailing.is_some() {
                        this.pending_terminal = trailing;
                    }
                    return if this.pending_write.is_some() {
                        this.end_after_write = true;
                        this.poll_usage_write(cx).unwrap_or(Poll::Pending)
                    } else if let Some(bytes) = this.pending_terminal.take() {
                        this.terminated = true;
                        Poll::Ready(Some(Ok(bytes)))
                    } else {
                        this.terminated = true;
                        Poll::Ready(None)
                    };
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl Drop for ResponsesSseTap {
    fn drop(&mut self) {
        // A body dropped before a terminal event means the caller disconnected.
        // Re-entering refreshes the exported end time; token usage is not
        // fabricated because no authoritative terminal usage was received.
        let span = self.span.clone();
        let _entered = span.enter();
        if !self.finalized {
            mark_client_cancelled(&self.span);
        }
        self.finalize(None);
        if let Some(future) = self.pending_write.take() {
            usage::detach_future(future);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Empty reasoning remains eligible for models without reasoning support.
    #[test]
    fn responses_requirements_cover_stream_images_tools_and_reasoning() {
        let request: ResponsesRequest = serde_json::from_value(serde_json::json!({
            "model": "custom/model",
            "stream": true,
            "input": [{
                "role": "user",
                "content": [{"type": "input_image", "image_url": "https://example.com/a.png"}]
            }],
            "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object"}}],
            "reasoning": {"effort": "medium"}
        }))
        .unwrap();
        let requirements = responses_requirements(&request);
        assert!(requirements
            .endpoint_features
            .contains(&EndpointFeature::Streaming));
        assert!(requirements
            .endpoint_features
            .contains(&EndpointFeature::ImageInput));
        assert!(requirements.features.contains(&ModelFeature::ToolCalling));
        assert!(requirements.features.contains(&ModelFeature::Reasoning));

        let mut profile = crate::capabilities::unknown_generation_capabilities("model");
        assert!(profile.satisfies(&requirements));
        profile
            .features
            .insert(ModelFeature::Reasoning, Support::Unsupported);
        assert!(!profile.satisfies(&requirements));

        for reasoning in [
            serde_json::json!({}),
            serde_json::json!({"effort": null}),
            serde_json::json!({"effort": "none"}),
            serde_json::json!({"effort": "none", "summary": null}),
        ] {
            let mut raw = request.raw().clone();
            raw["reasoning"] = reasoning;
            let request: ResponsesRequest = serde_json::from_value(raw).unwrap();
            let requirements = responses_requirements(&request);
            assert!(
                !requirements.features.contains(&ModelFeature::Reasoning),
                "{}",
                request.raw()
            );
            assert!(profile.satisfies(&requirements));
        }
        let mut raw = request.raw().clone();
        raw["reasoning"] = serde_json::json!({"effort": "none", "summary": "auto"});
        let request: ResponsesRequest = serde_json::from_value(raw).unwrap();
        assert!(responses_requirements(&request)
            .features
            .contains(&ModelFeature::Reasoning));
    }
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
    use opentelemetry_sdk::trace::{SimpleSpanProcessor, TracerProvider};
    use tracing_subscriber::layer::SubscriberExt;

    fn exported_span(
        name: &'static str,
        stamp: impl FnOnce(&LlmSpanWriter),
    ) -> opentelemetry_sdk::export::trace::SpanData {
        exported_span_with_output_limit(name, None, stamp)
    }

    fn exported_span_with_output_limit(
        name: &'static str,
        output_limit: Option<usize>,
        stamp: impl FnOnce(&LlmSpanWriter),
    ) -> opentelemetry_sdk::export::trace::SpanData {
        exported_span_with_options(name, output_limit, None, None, stamp)
    }

    fn exported_span_with_options(
        name: &'static str,
        output_limit: Option<usize>,
        log_content: Option<bool>,
        content_bytes: Option<usize>,
        stamp: impl FnOnce(&LlmSpanWriter),
    ) -> opentelemetry_sdk::export::trace::SpanData {
        crate::handlers::init_content_logging(true);
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("responses-test")));
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let span = tracing::info_span!("responses_test_span", otel.name = name);
            let span = match (output_limit, log_content, content_bytes) {
                (None, None, None) => LlmSpanWriter::new(span, None, LlmSpanKind::Llm),
                _ => LlmSpanWriter::new_with_limits(
                    span,
                    LlmSpanKind::Llm,
                    16,
                    output_limit.unwrap_or(64),
                    content_bytes
                        .unwrap_or(crate::handlers::trace_attributes::MAX_LLM_SPAN_CONTENT_BYTES),
                    log_content.unwrap_or(true),
                ),
            };
            stamp(&span);
            drop(span);
        }
        for result in provider.force_flush() {
            result.expect("span should flush");
        }
        exporter
            .get_finished_spans()
            .expect("finished spans")
            .into_iter()
            .find(|span| span.name.as_ref() == name)
            .expect("test span")
    }

    #[test]
    fn responses_and_compact_traces_keep_retained_order_and_numeric_tokens() {
        let wire = r#"{"model":"gpt-5","temperature":0.2000,"input":[{"type":"reasoning","encrypted_content":"hidden-secret","data":{"z":1.2300e-4,"a":2}}],"tools":[{"type":"function","name":"lookup","parameters":{"properties":{"z":{"minimum":1.2300e-4},"a":{}}}}],"metadata":{"secret":"metadata-secret"},"text":{"format":{"schema":{"secret":"schema-secret"}}}}"#;
        let native: ResponsesRequest = serde_json::from_str(wire).unwrap();
        let compact: crate::schema_json::SchemaJson = serde_json::from_str(wire).unwrap();
        for is_compact in [false, true] {
            let span = exported_span_with_options(
                "responses-retained-input",
                None,
                Some(true),
                None,
                |span| {
                    if is_compact {
                        set_input_value_span_attributes(span, &compact);
                    } else {
                        set_input_span_attributes(span, &native);
                    }
                },
            );
            let attributes: std::collections::HashMap<_, _> = span
                .attributes
                .into_iter()
                .map(|attribute| (attribute.key.to_string(), attribute.value.to_string()))
                .collect();
            let input = attributes
                .get("input.value")
                .expect("retained trace content");
            assert!(
                input.contains(r#""properties":{"z":{"minimum":1.2300e-4},"a":{}}"#),
                "{input}"
            );
            assert!(input.contains(r#""data":{"z":1.2300e-4,"a":2}"#));
            assert_eq!(
                attributes["llm.invocation_parameters"],
                r#"{"temperature":0.2000}"#
            );
            assert!(!attributes
                .values()
                .any(|value| value.contains("hidden-secret")
                    || value.contains("metadata-secret")
                    || value.contains("schema-secret")));
        }
    }

    #[test]
    fn retained_input_respects_content_logging_and_byte_budgets() {
        let input: crate::schema_json::SchemaJson = serde_json::from_str(r#"{"model":"gpt","temperature":0.2000,"input":"secret-prompt","tools":[{"secret":"tool-secret"}]}"#).unwrap();
        let disabled = exported_span_with_options(
            "responses-retained-disabled",
            None,
            Some(false),
            None,
            |span| set_input_value_span_attributes(span, &input),
        );
        assert!(!disabled
            .attributes
            .iter()
            .any(|attribute| attribute.key.as_str() == "input.value"
                || attribute.value.to_string().contains("secret")));
        assert!(disabled
            .attributes
            .iter()
            .any(
                |attribute| attribute.key.as_str() == "llm.invocation_parameters"
                    && attribute.value.to_string() == r#"{"temperature":0.2000}"#
            ));
        let limited = exported_span_with_options(
            "responses-retained-budget",
            None,
            Some(true),
            Some(8),
            |span| set_input_value_span_attributes(span, &input),
        );
        assert!(!limited
            .attributes
            .iter()
            .any(|attribute| attribute.key.as_str() == "input.value"));
        assert!(limited
            .attributes
            .iter()
            .any(|attribute| attribute.key.as_str() == "chalk.trace.input.truncation"));
    }

    #[test]
    fn trace_redaction_removes_encrypted_reasoning_recursively() {
        let input = serde_json::json!({
            "input": [{
                "type": "reasoning",
                "encrypted_content": "secret",
                "nested": {"encrypted_content": "also-secret", "safe": true}
            }]
        });
        let redacted = redact_encrypted_content(&input);
        assert_eq!(redacted["input"][0]["nested"]["safe"], true);
        assert!(redacted.to_string().find("secret").is_none());
        assert!(redacted.to_string().find("encrypted_content").is_none());
    }

    #[test]
    fn responses_messages_include_bounded_openinference_metadata() {
        let value = serde_json::json!({
            "output": [
                {"type": "reasoning", "summary": []},
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "hello"}]
                },
                {"type": "reasoning", "summary": []},
                {
                    "type": "function_call",
                    "call_id": "call-1",
                    "name": "lookup",
                    "arguments": "{\"secret\":true}"
                }
            ]
        });
        let messages = trace_messages(&value, true, false);
        let span =
            exported_span_with_output_limit("responses-message-attributes", Some(5), |span| {
                stamp_trace_messages(
                    span,
                    "llm.output_messages",
                    &messages,
                    ContentDirection::Output,
                );
            });
        let attributes = span
            .attributes
            .into_iter()
            .map(|attribute| (attribute.key.to_string(), attribute.value.to_string()))
            .collect::<std::collections::HashMap<_, _>>();
        let message_attributes = attributes
            .keys()
            .filter(|key| key.starts_with("llm.output_messages."))
            .count();
        assert!(message_attributes <= 5);
        assert_eq!(
            attributes
                .get("llm.output_messages.1.message.tool_calls.0.tool_call.function.name")
                .map(String::as_str),
            Some("lookup")
        );
        assert!(!attributes
            .keys()
            .any(|key| key.starts_with("llm.output_messages.2.")));
        assert!(attributes.contains_key("chalk.trace.output.truncation"));
    }

    #[test]
    fn responses_message_budget_keeps_the_newest_messages() {
        let value = serde_json::json!({
            "instructions": "old instructions",
            "input": [
                {"type": "message", "role": "user", "content": "older turn"},
                {"type": "message", "role": "user", "content": "newest turn"}
            ]
        });
        let messages = trace_messages(&value, true, true);
        let span = exported_span_with_output_limit("responses-newest-message", Some(2), |span| {
            stamp_trace_messages(
                span,
                "llm.output_messages",
                &messages,
                ContentDirection::Output,
            );
        });
        let attributes = span
            .attributes
            .into_iter()
            .map(|attribute| (attribute.key.to_string(), attribute.value.to_string()))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            attributes
                .get("llm.output_messages.0.message.content")
                .map(String::as_str),
            Some("newest turn")
        );
        assert!(!attributes.values().any(|value| value == "old instructions"));
        assert!(!attributes.values().any(|value| value == "older turn"));
    }

    #[test]
    fn compact_input_uses_safe_parameters_and_contiguous_messages() {
        let request = serde_json::json!({
            "model": "gpt-5",
            "service_tier": "priority",
            "temperature": 0.2,
            "metadata": {"secret": "do not flatten"},
            "input": [
                {"type": "reasoning", "encrypted_content": "opaque"},
                {"type": "message", "role": "user", "content": "hello"},
                {"type": "message", "role": "assistant", "content": "world"}
            ]
        });
        let span = exported_span("responses-compact-input", |span| {
            set_input_value_span_attributes(span, &request.clone().into());
        });
        let attributes = span
            .attributes
            .into_iter()
            .map(|attribute| (attribute.key.to_string(), attribute.value.to_string()))
            .collect::<std::collections::HashMap<_, _>>();
        let parameters: Value = serde_json::from_str(
            attributes
                .get("llm.invocation_parameters")
                .expect("safe invocation parameters"),
        )
        .unwrap();
        assert_eq!(parameters["service_tier"], "priority");
        assert_eq!(parameters["temperature"], 0.2);
        assert!(parameters.get("metadata").is_none());
        assert_eq!(
            attributes
                .get("llm.input_messages.0.message.role")
                .map(String::as_str),
            Some("user")
        );
        assert_eq!(
            attributes
                .get("llm.input_messages.1.message.role")
                .map(String::as_str),
            Some("assistant")
        );
        assert!(!attributes
            .keys()
            .any(|key| key.starts_with("llm.input_messages.2.")));
    }

    #[test]
    fn metadata_only_messages_never_collect_content_or_arguments() {
        let value = serde_json::json!({
            "input": [
                {"role": "user", "content": "secret prompt"},
                {
                    "type": "function_call",
                    "call_id": "call-1",
                    "name": "lookup",
                    "arguments": "secret arguments"
                }
            ]
        });
        let messages = trace_messages(&value, false, true);
        assert!(messages.iter().all(|message| message.content.is_none()));
        assert!(messages
            .iter()
            .flat_map(|message| &message.tool_calls)
            .all(|tool_call| tool_call.arguments.is_none()));
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].tool_calls[0].name, "lookup");
    }

    #[test]
    fn ttft_ignores_lifecycle_and_empty_delta_events() {
        let created = serde_json::json!({"type": "response.created"});
        assert!(!is_content_bearing_delta(
            Some("response.created"),
            Some(&created)
        ));
        let empty = serde_json::json!({"delta": ""});
        assert!(!is_content_bearing_delta(
            Some("response.output_text.delta"),
            Some(&empty)
        ));
        let tool = serde_json::json!({"delta": "{\"city\":"});
        assert!(is_content_bearing_delta(
            Some("response.function_call_arguments.delta"),
            Some(&tool)
        ));
        let reasoning = serde_json::json!({"delta": "summary"});
        assert!(is_content_bearing_delta(
            Some("response.reasoning_summary_text.delta"),
            Some(&reasoning)
        ));
    }

    #[test]
    fn client_cancellation_sets_error_status_and_attributes() {
        let span = exported_span("responses-client-cancelled", mark_client_cancelled);
        let attributes = span
            .attributes
            .into_iter()
            .map(|attribute| (attribute.key.to_string(), attribute.value.to_string()))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            attributes.get("llm.finish_reason").map(String::as_str),
            Some("cancelled")
        );
        assert_eq!(
            attributes.get("error.type").map(String::as_str),
            Some("client_cancelled")
        );
        assert!(matches!(
            span.status,
            opentelemetry::trace::Status::Error { .. }
        ));
    }

    #[test]
    fn terminal_completed_event_extracts_usage_without_double_counting_reasoning() {
        let data = serde_json::json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 8,
                    "output_tokens_details": {"reasoning_tokens": 6},
                    "total_tokens": 18
                }
            }
        });
        let terminal = terminal_event("response.completed", &data).expect("terminal event");
        let usage = TokenUsage::from_responses_value(&terminal.usage.expect("usage")).unwrap();
        assert_eq!(terminal.status, "completed");
        assert_eq!(usage.output_tokens, 8);
        assert_eq!(usage.reasoning_tokens, 6);
        assert_eq!(usage.total_tokens, 18);
    }

    #[test]
    fn standalone_error_is_not_treated_as_terminal() {
        let data = serde_json::json!({
            "type": "error",
            "sequence_number": 3,
            "error": {"message": "recoverable event before response.failed"}
        });
        assert!(terminal_event("error", &data).is_none());
    }

    #[test]
    fn repeated_upstream_errors_stamp_only_the_first_error() {
        let first = serde_json::json!({"type": "error", "message": "first"});
        let second = serde_json::json!({"type": "error", "message": "second"});
        let span = exported_span("responses-upstream-error", |span| {
            let mut seen = false;
            stamp_upstream_error_once(span, Some(&first), &mut seen);
            stamp_upstream_error_once(span, Some(&second), &mut seen);
            assert!(seen);
        });
        let error_messages = span
            .attributes
            .iter()
            .filter(|attribute| attribute.key.as_str() == "error.message")
            .collect::<Vec<_>>();
        assert_eq!(error_messages.len(), 1);
        assert_eq!(error_messages[0].value.to_string(), "first");
    }

    #[test]
    fn transport_error_is_a_named_responses_event() {
        let bytes = transport_error_event(&BackendError::Other("broken pipe".to_string()), 42);
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.starts_with("event: error\ndata: "));
        assert!(text.ends_with("\n\n"));
        assert!(text.contains("stream_transport_error"));
        assert!(text.contains("\"sequence_number\":42"));
        let payload: Value = serde_json::from_str(
            text.lines()
                .find_map(|line| line.strip_prefix("data: "))
                .expect("data line"),
        )
        .unwrap();
        assert_eq!(payload["type"], "error");
        assert_eq!(payload["code"], "stream_transport_error");
        assert!(payload.get("message").and_then(Value::as_str).is_some());
        assert!(payload.get("param").is_some_and(Value::is_null));
        assert!(payload.get("error").is_none());
    }
}
