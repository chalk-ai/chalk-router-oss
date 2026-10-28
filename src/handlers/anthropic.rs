use crate::tenancy::RequestState as State;
use std::collections::BTreeMap;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::api_keys::{ApiKeyError, ApiKeyPermission};
use crate::backend::provider::{
    forwardable_upstream_headers, AnthropicRequestEndpoint, AnthropicRequestHeaders, BackendError,
    ResolvedRoute, RouteResolution,
};
use crate::capabilities::{
    CapabilityRequirements, DeliveryMode, EndpointFeature, InputModality, ModelFeature,
    ModelSurface, OutputModality, Protocol, ProtocolOperation, Support,
};
use crate::middleware::ResolvedApiKey;
use crate::prompt_cache::{CacheContext, CachedCompletion, PayloadKind, Surface};
use crate::quota::{QuotaPermit, TokenReservation};
use crate::ratelimit::RateLimitModel;
use axum::{
    body::{Body, Bytes},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures::Stream;

use super::routing::{
    acquire_rate_limit, acquire_request_quota, acquire_single_route_quota,
    handle_raw_upstream_dispatch, raw_upstream_failure_response, set_route_span_attributes,
    stamp_routing_headers, try_with_fallbacks_if, RawUpstreamFailure, RoutedRequest,
};
use super::sse::{encode_frame, SseDecoder, SseFrame};
use super::trace_attributes::{
    ContentDirection, LlmSpanKind, LlmSpanWriter, MAX_INVOCATION_PARAMETERS_BYTES,
};
use super::usage::{self, TokenUsage, UsageAttribution, UsageWriteFuture};
use crate::usage_estimator::{NormalizedUsageInput, ToolContext, UsageEstimateState};

const ANTHROPIC_INVOCATION_PARAMETER_PRIORITY: &[&str] = &[
    "max_tokens",
    "temperature",
    "top_p",
    "top_k",
    "stop_sequences",
    "tool_choice",
    "thinking",
    "service_tier",
];

fn bounded_anthropic_invocation_parameters(
    request: &serde_json::Map<String, serde_json::Value>,
) -> (Option<String>, usize) {
    let keys = ANTHROPIC_INVOCATION_PARAMETER_PRIORITY
        .iter()
        .copied()
        .filter(|key| request.contains_key(*key))
        .collect::<Vec<_>>();

    let mut parameters = serde_json::Map::new();
    let mut serialized = None;
    let mut omitted = 0usize;
    for key in keys {
        let Some(value) = request.get(key) else {
            continue;
        };
        parameters.insert(key.to_string(), value.clone());
        match serde_json::to_string(&parameters) {
            Ok(candidate) if candidate.len() <= MAX_INVOCATION_PARAMETERS_BYTES => {
                serialized = Some(candidate);
            }
            _ => {
                parameters.remove(key);
                omitted += 1;
            }
        }
    }
    (serialized, omitted)
}

/// The `model` the request body names, if any. Anthropic requires it on both
/// `/v1/messages` and `/v1/messages/count_tokens`; a body we can't parse or that
/// omits it yields `None` (which fails a model-scoped key closed — see
/// [`check_permission`]).
fn request_model(body: &Bytes) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("model")?
        .as_str()
        .map(str::to_string)
}

/// Replace only the model field before native dispatch. Unknown Anthropic
/// request fields remain intact so the passthrough stays forward-compatible.
fn rewrite_request_model(body: &Bytes, upstream_model: &str) -> Result<Bytes, String> {
    let mut value: crate::schema_json::SchemaJson = serde_json::from_slice(body)
        .map_err(|error| format!("invalid Anthropic request JSON: {error}"))?;
    if !value.is_object() {
        return Err("Anthropic request body must be a JSON object".to_string());
    }
    value
        .set_member(
            "model",
            serde_json::Value::String(upstream_model.to_string()).into(),
        )
        .map_err(|error| error.to_string())?;
    serde_json::to_vec(&value)
        .map(Bytes::from)
        .map_err(|error| format!("failed to encode Anthropic request: {error}"))
}

/// Gate the passthrough on the presented key's permission before it can spend
/// against Anthropic. The middleware only *authenticates*, so without this a key
/// scoped to another provider / connection / model could still reach Anthropic.
/// The provider kind is always `"anthropic"`, the connection is the resolved
/// one, and `model` is read from the body — absent ⇒ `""`, so a model-scoped key
/// fails closed. Unrestricted keys skip the check.
fn check_permission(
    permission: &ApiKeyPermission,
    resolution: &RouteResolution,
    model: Option<&str>,
) -> Result<(), ApiKeyError> {
    if !permission.is_restricted() {
        return Ok(());
    }
    permission.allows_route(resolution, model.unwrap_or(""))
}

/// Extract text from an Anthropic content value: a plain string, or an array
/// of content blocks whose `text` fields are joined with `"\n"`.
/// `filter_text_type` restricts the join to `{"type":"text"}` blocks — needed
/// for `messages[].content`, which can also carry non-text blocks (images,
/// tool_use/tool_result); the `system` prompt's block array is text-only in
/// practice, so callers pass `false` there (mirrors `message_content_to_string`
/// in chat.rs, extended for Anthropic's block-array shape).
fn anthropic_content_to_string(
    content: &serde_json::Value,
    filter_text_type: bool,
) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(blocks) => {
            let text: String = blocks
                .iter()
                .filter(|b| {
                    !filter_text_type || b.get("type").and_then(|t| t.as_str()) == Some("text")
                })
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

/// Stamp OpenInference request attributes for `/v1/messages` onto the current
/// span, parsing the raw body as JSON. Called only on the LLM-instrumented
/// path (`proxy_messages`), and as early as possible — right after connection
/// resolution, before the permission gate and dispatch — so a denied or
/// errored request still carries system/model/connection/input, matching
/// `set_route_span_attributes` / `set_input_span_attributes` in chat.rs.
///
/// The `openinference.span.kind` / `llm.system` / `llm.provider` /
/// `chalk.connection_id` attrs don't depend on the body parsing, so they're
/// always set; a body that isn't valid JSON just means the rest are skipped
/// (the upstream will reject it anyway).
fn set_anthropic_request_span_attributes(
    span: &LlmSpanWriter,
    body: &Bytes,
    resolution: &RouteResolution,
) {
    span.set_route(resolution);

    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return;
    };
    let Some(obj) = value.as_object() else { return };

    let is_stream = obj.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    span.set_stream(is_stream);

    let log_content = super::content_logging_enabled();

    // Sampling parameters are metadata, matching the OpenAI frontend, so they
    // remain visible when content logging is disabled. Only explicitly
    // recognized generation parameters are admitted: arbitrary extensions
    // can contain client content such as `metadata.user_id`. This also avoids
    // cloning messages.
    let (invocation_parameters, invocation_parameters_omitted) =
        bounded_anthropic_invocation_parameters(obj);
    if let Some(invocation_parameters) = invocation_parameters {
        span.set_invocation_parameters_json(invocation_parameters);
    }

    // Anthropic hoists the system prompt out of `messages` into its own top-
    // level field; OpenInference has no separate slot for it, so it's emitted
    // as input message 0 with role "system" (matching how chat.rs's OpenAI
    // shape represents it as a regular message).
    let mut messages = Vec::new();
    if let Some(system) = obj.get("system") {
        messages.push((
            "system".to_string(),
            anthropic_content_to_string(system, false),
        ));
    }
    if let Some(raw_messages) = obj.get("messages").and_then(|m| m.as_array()) {
        for msg in raw_messages {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = msg
                .get("content")
                .and_then(|content| anthropic_content_to_string(content, true));
            messages.push((role.to_string(), content));
        }
    }

    if log_content {
        std::str::from_utf8(body)
            .ok()
            .map(str::to_owned)
            .and_then(|json| span.record_json_string(ContentDirection::Input, json));
    }
    if let Some(requested_model) = obj.get("model").and_then(|model| model.as_str()) {
        let model = &resolution.upstream_model_id;
        if requested_model != model {
            span.set_requested_served_models(requested_model, model);
        }
    }
    if invocation_parameters_omitted > 0 {
        span.truncation(
            ContentDirection::Input,
            "invocation_parameters",
            serde_json::json!({
                "max_bytes": MAX_INVOCATION_PARAMETERS_BYTES,
                "fields_omitted": invocation_parameters_omitted,
            }),
        );
    }
    let tools = obj.get("tools").and_then(|tools| tools.as_array());
    let total_tools = tools.map_or(0, Vec::len);
    let reserved_tool_attributes = if log_content && total_tools > 0 {
        let (input_limit, _) = super::trace_attributes::llm_attribute_budgets();
        total_tools.min(input_limit / 3)
    } else {
        0
    };
    let reserved_tool_content_bytes = if reserved_tool_attributes > 0 {
        span.content_bytes_remaining(ContentDirection::Input) / 2
    } else {
        0
    };
    let mut planned = Vec::new();
    let mut content_omitted = 0usize;
    // Spend the message budget newest-to-oldest without renumbering retained
    // OpenInference source indices.
    let message_order = (0..messages.len()).rev();
    for source_index in message_order {
        let (role, content) = &messages[source_index];
        if !span.try_metadata_preserving(
            ContentDirection::Input,
            format!("llm.input_messages.{source_index}.message.role"),
            role.clone(),
            reserved_tool_attributes,
            reserved_tool_content_bytes,
        ) {
            continue;
        }
        let record_content = log_content
            && content.as_ref().is_some_and(|text| {
                span.try_content_preserving(
                    ContentDirection::Input,
                    format!("llm.input_messages.{source_index}.message.content"),
                    text.clone(),
                    reserved_tool_attributes,
                    reserved_tool_content_bytes,
                )
            });
        if log_content && content.is_some() && !record_content {
            content_omitted += 1;
        }
        planned.push((source_index, role, content, record_content));
    }
    if planned.len() < messages.len() || content_omitted > 0 {
        let source_start_index = planned.iter().map(|(index, _, _, _)| *index).min();
        let source_end_index = planned.iter().map(|(index, _, _, _)| *index).max();
        let retained_suffix_start = planned
            .iter()
            .map(|(index, _, _, _)| *index)
            .min()
            .unwrap_or(messages.len());
        span.truncation(
            ContentDirection::Input,
            "messages",
            serde_json::json!({
                "total": messages.len(),
                "recorded": planned.len(),
                "omitted": messages.len().saturating_sub(planned.len()),
                "omitted_before": retained_suffix_start,
                "source_start_index": source_start_index,
                "source_end_index": source_end_index,
                "content_fields_omitted": content_omitted,
            }),
        );
    }

    if log_content {
        let mut recorded = 0usize;
        if let Some(tools) = tools {
            for (source_index, tool) in tools.iter().enumerate() {
                let Ok(json) = serde_json::to_string(tool) else {
                    continue;
                };
                if !span.try_content(
                    ContentDirection::Input,
                    format!("llm.tools.{source_index}.tool.json_schema"),
                    json,
                ) {
                    continue;
                }
                recorded += 1;
            }
        }
        if recorded < total_tools {
            span.truncation(
                ContentDirection::Input,
                "tools",
                serde_json::json!({"total": total_tools, "recorded": recorded}),
            );
        }
    }
}

/// Stamp OpenInference output attributes from an Anthropic Messages response
/// JSON (`{"role":...,"content":[...],"usage":{...}}`). Shared by the
/// non-streaming path (parsed straight from the response body) and the
/// streaming path (assembled by [`AnthropicSseSpanTap`] from the SSE deltas
/// into the same shape) — mirroring how chat.rs's `set_output_span_attributes`
/// serves both of its paths.
fn set_anthropic_output_span_attributes(span: &LlmSpanWriter, response: &serde_json::Value) {
    // Usage and finish metadata must precede expandable output fields because
    // the SDK retains the first N attributes when its limit is reached.
    if let Some(usage) = response.get("usage") {
        let tokens = anthropic_usage_tokens(usage);
        let prompt = tokens.prompt_tokens() as i64;
        let completion = tokens.output_tokens as i64;
        span.set_token_usage(
            prompt as u64,
            Some(completion as u64),
            (prompt + completion) as u64,
            tokens.cache_read_input_tokens,
            tokens.cache_creation_input_tokens,
            0,
        );
    }
    if let Some(stop_reason) = response.get("stop_reason").and_then(|value| value.as_str()) {
        span.set_finish_reason(stop_reason);
    }

    let log_content = super::content_logging_enabled();
    if log_content {
        span.record_json(ContentDirection::Output, response);
    }

    let role = response
        .get("role")
        .and_then(|r| r.as_str())
        .unwrap_or("assistant");
    let role_recorded = span.try_metadata(
        ContentDirection::Output,
        "llm.output_messages.0.message.role",
        role,
    );

    let blocks = response
        .get("content")
        .and_then(|content| content.as_array());
    let total_tool_calls = blocks.map_or(0, |blocks| {
        blocks
            .iter()
            .filter(|block| block.get("type").and_then(|value| value.as_str()) == Some("tool_use"))
            .count()
    });
    let mut recorded_tool_calls = Vec::new();
    if let Some(blocks) = blocks {
        let mut tool_source_index = 0usize;
        for block in blocks {
            if block.get("type").and_then(|value| value.as_str()) != Some("tool_use") {
                continue;
            }
            let index = tool_source_index;
            tool_source_index += 1;
            let id = block.get("id").and_then(|value| value.as_str());
            let name = block.get("name").and_then(|value| value.as_str());
            let metadata_fields = usize::from(id.is_some()) + usize::from(name.is_some());
            if metadata_fields == 0 {
                continue;
            }
            let fields = id
                .map(|id| {
                    (
                        format!("llm.output_messages.0.message.tool_calls.{index}.tool_call.id"),
                        id.to_string(),
                    )
                })
                .into_iter()
                .chain(name.map(|name| {
                    (
                        format!("llm.output_messages.0.message.tool_calls.{index}.tool_call.function.name"),
                        name.to_string(),
                    )
                }));
            if !span.try_metadata_group(ContentDirection::Output, fields) {
                continue;
            }
            let arguments = block
                .get("input")
                .and_then(|input| serde_json::to_string(input).ok());
            recorded_tool_calls.push((index, arguments));
        }
    }

    let mut omitted_arguments = if log_content {
        total_tool_calls.saturating_sub(recorded_tool_calls.len())
    } else {
        0
    };
    if log_content {
        for (index, arguments) in &recorded_tool_calls {
            if let Some(arguments) = arguments {
                if !span.try_content(
                    ContentDirection::Output,
                        format!("llm.output_messages.0.message.tool_calls.{index}.tool_call.function.arguments"),
                        arguments.clone(),
                    ) {
                    omitted_arguments += 1;
                }
            }
        }
    }

    let mut content_omitted = 0usize;
    if log_content {
        if let Some(blocks) = blocks {
            let text: String = blocks
                .iter()
                .filter(|block| block.get("type").and_then(|value| value.as_str()) == Some("text"))
                .filter_map(|block| block.get("text").and_then(|value| value.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty()
                && !span.try_content(
                    ContentDirection::Output,
                    "llm.output_messages.0.message.content",
                    text,
                )
            {
                content_omitted = 1;
            }
        }
    }

    if !role_recorded || content_omitted > 0 {
        span.truncation(
            ContentDirection::Output,
            "messages",
            serde_json::json!({
                "total": 1,
                "roles_recorded": usize::from(role_recorded),
                "content_fields_omitted": content_omitted,
            }),
        );
    }
    if recorded_tool_calls.len() < total_tool_calls || omitted_arguments > 0 {
        span.truncation(
            ContentDirection::Output,
            "tool_calls",
            serde_json::json!({
                "total": total_tool_calls,
                "recorded": recorded_tool_calls.len(),
                "arguments_omitted": omitted_arguments,
            }),
        );
    }
}

/// The four token-count fields an Anthropic `usage` JSON value can carry.
/// Shared by the non-streaming response's top-level `usage` and the streaming
/// path's per-event `usage` objects (`message_start.message.usage`,
/// `message_delta.usage`) — each of those carries only a subset of the four
/// fields, so a missing field always defaults to 0 rather than erroring.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct AnthropicUsageTokens {
    input_tokens: u64,
    output_tokens: u64,
    /// Tokens served from the prompt cache (a subset-of-prompt marker, like
    /// OpenAI's `cached_tokens` — NOT additional to `input_tokens`).
    cache_read_input_tokens: u64,
    /// Tokens written to the prompt cache on this request. Anthropic's
    /// `input_tokens` EXCLUDES both this and `cache_read_input_tokens`, so
    /// omitting either from the prompt total undercounts cached requests
    /// (and would let token budgets be bypassed).
    cache_creation_input_tokens: u64,
}

impl AnthropicUsageTokens {
    /// True total input tokens: `input_tokens` plus both cache fields, which
    /// Anthropic reports as separate, non-overlapping counts.
    fn prompt_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_input_tokens)
            .saturating_add(self.cache_creation_input_tokens)
    }
}

fn anthropic_usage_tokens(usage: &serde_json::Value) -> AnthropicUsageTokens {
    let field = |name: &str| usage.get(name).and_then(|v| v.as_u64()).unwrap_or(0);
    AnthropicUsageTokens {
        input_tokens: field("input_tokens"),
        output_tokens: field("output_tokens"),
        cache_read_input_tokens: field("cache_read_input_tokens"),
        cache_creation_input_tokens: field("cache_creation_input_tokens"),
    }
}

/// Record token usage for a completed `/v1/messages` response: per-key
/// bookkeeping plus the durable usage ledger, mirroring `handle_non_streaming`'s
/// usage block in chat.rs. The resolved route supplies the provider, connection,
/// and provider-native model; unlike the OpenAI path, `cached_tokens` is
/// populated because Anthropic reports it. A no-op when neither token count is
/// reported.
async fn record_anthropic_usage(
    state: &crate::AppState,
    api_key: &ResolvedApiKey,
    resolution: &RouteResolution,
    span: &LlmSpanWriter,
    response: &serde_json::Value,
    normalized_usage_input: Option<NormalizedUsageInput>,
    permit: Option<QuotaPermit>,
) -> Result<TokenUsage, crate::usage_tracker::UsageWriteError> {
    let reported = response.get("usage").and_then(|usage| {
        usage.get("input_tokens")?.as_u64()?;
        usage.get("output_tokens")?.as_u64()?;
        let tokens = anthropic_usage_tokens(usage);
        let prompt_tokens = tokens.prompt_tokens();
        let completion_tokens = tokens.output_tokens;
        Some(TokenUsage {
            input_tokens: prompt_tokens,
            output_tokens: completion_tokens,
            output_tokens_observed: true,
            cached_input_tokens: tokens.cache_read_input_tokens,
            cache_write_input_tokens: tokens.cache_creation_input_tokens,
            reasoning_tokens: 0,
            total_tokens: prompt_tokens.saturating_add(completion_tokens),
            usage_source: crate::usage_tracker::UsageSource::Reported,
            ..TokenUsage::default()
        })
    });
    let estimated = || {
        let mut estimate = UsageEstimateState::new(resolution, normalized_usage_input)?;
        estimate.observe_json(response);
        Some(TokenUsage::from_estimate(estimate))
    };
    let token_usage =
        reported
            .or_else(estimated)
            .ok_or(crate::usage_tracker::UsageWriteError::Invalid(
                "Anthropic response had neither valid reported usage nor a supported estimator",
            ))?;
    token_usage.stamp(span);
    usage::with_quota_settlement(
        usage::record_future(
            state.clone(),
            UsageAttribution::new(api_key, resolution),
            token_usage,
        ),
        permit,
        Some(token_usage.total_tokens),
    )
    .await?;
    Ok(token_usage)
}

// ---------------------------------------------------------------------------
// Exact-prefix response cache
// ---------------------------------------------------------------------------
//
// This surface is a byte passthrough, so its cache is too: the stored value is
// the upstream response exactly as it arrived — a JSON body, or a whole SSE
// stream — and a hit replays those bytes unchanged. That is what makes caching
// work here at all without the router having to understand, re-serialize, or
// re-chunk the Messages wire format.
//
// The consequence, unlike the OpenAI chat surface (which stores one canonical
// completion and re-emits it in either shape), is that streaming and
// non-streaming do NOT share entries: `stream` is part of the request body, so
// it is part of the digest, and the two forms are genuinely different stored
// values.
//
// Only `/v1/messages` is cached. `/v1/messages/count_tokens` is not a
// generation, is already cheap, and must stay byte-for-byte a passthrough.

/// Serve a cached Messages response by replaying its bytes.
///
/// A JSON entry is re-parsed only to stamp the span's output attributes, so a
/// cached turn traces like a live one; the bytes handed to the client are the
/// stored ones either way. SSE bytes are decoded side-effect-free to assemble
/// span output and evaluate outcome rules; they are never passed through the
/// live tap, so replay cannot record usage or alter the response bytes.
fn cache_hit_response(
    span: &LlmSpanWriter,
    sampling_gate: &std::sync::Arc<crate::telemetry::sampling::SamplingGate>,
    backend: &'static str,
    ctx: &CacheContext,
    value: &CachedCompletion,
) -> Response {
    // This request replays router-owned bytes and never reaches the provider.
    // Keep the exact zero upstream cost even if cached usage is added to these
    // spans in the future.
    span.suppress_provider_cost_for_cache_hit();
    let content_type = match value.payload_kind {
        PayloadKind::Json => "application/json",
        PayloadKind::Sse => "text/event-stream",
    };
    let response_for_span = match value.payload_kind {
        PayloadKind::Json => serde_json::from_slice::<serde_json::Value>(&value.payload).ok(),
        PayloadKind::Sse => anthropic_response_from_cached_sse(&value.payload),
    };
    if let Some(response) = response_for_span {
        stamp_anthropic_response(span, sampling_gate, &response);
    }
    let mut res = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(value.payload.clone()))
        .unwrap();
    crate::handlers::stamp_trace_headers(res.headers_mut());
    crate::handlers::stamp_cache_outcome(
        span,
        Some(res.headers_mut()),
        backend,
        ctx,
        Some(value.age()),
    );
    res
}

pub async fn proxy_messages(
    State(state): State<crate::AppState>,
    api_key: ResolvedApiKey,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // `count_tokens` shares `proxy_request` (below) but must not get LLM span
    // attributes or a tap — token counting isn't a generation, and it has no
    // fallback semantics. Only this path wraps the call in the
    // `anthropic.messages` span and dispatches through the model's configured
    // fallbacks.
    let span = tracing::info_span!("anthropic_messages", otel.name = "anthropic.messages");
    let span = LlmSpanWriter::new(span, state.object_store.clone(), LlmSpanKind::Llm);
    super::stamp_authenticated_principal(&span, &api_key);
    span.clone()
        .instrument(async move {
            let start = std::time::Instant::now();
            proxy_messages_with_fallback(&state, &api_key, &headers, body, span, start).await
        })
        .await
}

/// The subset of an Anthropic `/v1/messages` request [`try_with_fallbacks_if`]
/// needs to evaluate and dispatch each candidate route: the raw body (rewritten
/// in place with the upstream model id once a route is chosen), the forwarded
/// `anthropic-*` headers, and the parsed body/usage-estimate views computed
/// once up front from the client's original request (capability requirements
/// and token estimates don't depend on which route ultimately serves it).
#[derive(Clone)]
struct AnthropicMessagesRequest {
    body: Bytes,
    headers: AnthropicRequestHeaders,
    request_value: serde_json::Value,
    admission_input: Option<NormalizedUsageInput>,
    soft_cap: bool,
}

impl RoutedRequest for AnthropicMessagesRequest {
    fn set_model(&mut self, model: String) {
        match rewrite_request_model(&self.body, &model) {
            Ok(body) => self.body = body,
            Err(error) => tracing::warn!(
                error = %error,
                "failed to rewrite Anthropic request model before dispatch; sending original body"
            ),
        }
    }

    fn check_capability(&self, route: &ResolvedRoute) -> Result<(), String> {
        let requirements =
            anthropic_requirements(AnthropicRequestEndpoint::Messages, &self.request_value);
        let model = self
            .request_value
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        crate::capabilities::trace_decision(
            route,
            model,
            &requirements,
            "anthropic_messages.create",
        );
        (anthropic_messages_mode(route) != DeliveryMode::Unsupported
            && route.capabilities.profile.satisfies(&requirements))
        .then_some(())
        .ok_or_else(|| super::routing::NO_LOSSLESS_ROUTE.to_string())
    }

    fn capability_support(&self, route: &ResolvedRoute) -> Support {
        route
            .capabilities
            .profile
            .support_for(&anthropic_requirements(
                AnthropicRequestEndpoint::Messages,
                &self.request_value,
            ))
    }

    fn conservative_input_tokens(&self) -> Option<u64> {
        // Already computed once up front when this request was built (see the
        // struct docs above), so this is a cheap field read rather than a
        // fresh JSON walk.
        self.admission_input
            .as_ref()
            .map(NormalizedUsageInput::conservative_tokens)
    }

    fn soft_cap(&self) -> bool {
        self.soft_cap
    }

    fn rate_limit_tokens(
        &self,
        route: &ResolvedRoute,
        input_tokens: Option<u64>,
    ) -> (Option<u64>, Option<u64>) {
        let requested_output = self
            .request_value
            .get("max_tokens")
            .and_then(serde_json::Value::as_u64);
        let output_tokens =
            requested_output.or(route.capabilities.profile.limits.max_output_tokens);
        (
            input_tokens
                .zip(output_tokens)
                .map(|(i, o)| i.saturating_add(o)),
            input_tokens,
        )
    }
}

/// The visible input that bounds admission, and whether the provider reads
/// input beyond it: server tools or MCP servers, or opaque content such as
/// images and documents. Either makes the admission a soft cap and rules out a
/// missing-usage estimate from this input.
fn messages_admission_input(
    request_value: &serde_json::Value,
) -> (Option<NormalizedUsageInput>, bool) {
    (
        NormalizedUsageInput::visible_json(request_value),
        ToolContext::of_messages(request_value) == ToolContext::Hosted
            || crate::usage_estimator::provider_adds_input(request_value),
    )
}

fn anthropic_messages_mode(route: &ResolvedRoute) -> DeliveryMode {
    route
        .capabilities
        .profile
        .operation(Protocol::AnthropicMessages, ProtocolOperation::Create)
        .map_or(DeliveryMode::Unknown, |operation| operation.mode)
}

/// The two shapes a dispatched `/v1/messages` attempt can come back in: a raw
/// upstream response (native or header/body-adapted delivery, both proxied as
/// bytes) or an already-materialized translation through a Chat Completions
/// backend. Mirrors the two return paths `proxy_request` handles inline for a
/// single route, now produced once per fallback candidate.
enum AnthropicDispatchOutcome {
    Native(reqwest::Response),
    Translated(crate::adapters::anthropic_chat::TranslatedAnthropicResponse),
}

// Takes an owned backend handle and delivery mode (extracted from the
// `&ResolvedRoute` synchronously by the caller) rather than borrowing the
// route itself: `try_with_fallbacks_if`'s dispatch closure is a higher-ranked
// `Fn(&ResolvedRoute, R) -> Fut` bound, and an `async fn` that borrows its
// `&ResolvedRoute` parameter across an `.await` produces a future type tied to
// that specific borrow's lifetime, which fails the "same `Fut` for every
// lifetime" HRTB check (see the `responses.rs` dispatch closures, which follow
// the same owned-extraction pattern for the same reason).
async fn dispatch_anthropic_messages(
    backend: std::sync::Arc<dyn crate::backend::BackendProvider>,
    mode: DeliveryMode,
    request: AnthropicMessagesRequest,
    raw_failure: &std::sync::Mutex<Option<RawUpstreamFailure>>,
) -> Result<AnthropicDispatchOutcome, BackendError> {
    match mode {
        DeliveryMode::Translated | DeliveryMode::Unknown => {
            let result = crate::adapters::anthropic_chat::translate_messages(
                backend.as_ref(),
                &request.body,
            )
            .await
            .map(AnthropicDispatchOutcome::Translated);
            // This candidate's failure (if any) didn't come from a native/adapted
            // HTTP response, so any stale raw passthrough stashed by an earlier
            // candidate no longer describes the most recent attempt; drop it so
            // the final error, if this ends up being the last one tried, falls
            // back to the synthesized error response instead of forwarding
            // someone else's bytes.
            if result.is_err() {
                *raw_failure.lock().unwrap() = None;
            }
            result
        }
        DeliveryMode::Native => handle_raw_upstream_dispatch(
            backend
                .send_anthropic_request(
                    AnthropicRequestEndpoint::Messages,
                    &request.headers,
                    request.body,
                )
                .await,
            raw_failure,
        )
        .await
        .map(AnthropicDispatchOutcome::Native),
        DeliveryMode::Adapted => handle_raw_upstream_dispatch(
            backend
                .send_anthropic_adapted_request(
                    AnthropicRequestEndpoint::Messages,
                    &request.headers,
                    request.body,
                )
                .await,
            raw_failure,
        )
        .await
        .map(AnthropicDispatchOutcome::Native),
        DeliveryMode::Unsupported => {
            unreachable!("capability_eligible excludes Unsupported routes")
        }
    }
}

/// Forward `/v1/messages` to the resolved primary route, falling back through
/// the model's configured alternates (via [`try_with_fallbacks_if`]) when the
/// primary is capability-ineligible, rate-limited, or the dispatch itself
/// fails. Permission checks, the exact-prefix cache, and response building for
/// the winning candidate mirror `proxy_request`'s single-route logic exactly;
/// only the fallback search in between is new.
async fn proxy_messages_with_fallback(
    state: &crate::AppState,
    api_key: &ResolvedApiKey,
    headers: &HeaderMap,
    body: Bytes,
    span: LlmSpanWriter,
    start: std::time::Instant,
) -> Response {
    tracing::debug!(path = "/v1/messages", "Proxying Anthropic API request");

    let req_model = request_model(&body);
    let Some(public_model) = req_model.as_deref() else {
        return (
            StatusCode::BAD_REQUEST,
            "Anthropic request requires a string `model` field",
        )
            .into_response();
    };

    let route = match state.backend.resolve_anthropic(public_model).await {
        Ok(route) => route,
        Err(error) => {
            return crate::handlers::backend_error_response_for_format(
                &error,
                crate::backend::provider::UpstreamErrorFormat::Other,
            )
        }
    };

    set_anthropic_request_span_attributes(&span, &body, &route.resolution);

    if let Err(denied) = check_permission(
        &api_key.0.permission,
        &route.resolution,
        req_model.as_deref(),
    ) {
        tracing::warn!(
            key_prefix = %api_key.0.key_prefix,
            path = "/v1/messages",
            reason = %denied,
            "Anthropic passthrough denied by API key permission"
        );
        return crate::handlers::forbidden_response(denied.to_string());
    }

    // Exact-prefix cache, consulted after the permission gate and before any
    // route (primary or fallback) is dispatched — see `proxy_request`'s cache
    // block for the full rationale; unchanged here.
    let cache_ctx = if route.routing.is_none() {
        state.raw_body_cache_context(&api_key.0, Surface::AnthropicMessages, headers, &body)
    } else {
        None
    };
    if let Some(ctx) = &cache_ctx {
        if let Some(value) = state.prompt_cache.get(&ctx.key).await {
            let cache_permit = match acquire_request_quota(
                state,
                api_key,
                TokenReservation::NotApplicable,
            )
            .await
            {
                Ok(permit) => permit,
                Err(error) => {
                    return crate::handlers::backend_error_response_for_format(
                        &error,
                        crate::backend::provider::UpstreamErrorFormat::Other,
                    )
                }
            };
            if let Some(permit) = cache_permit {
                permit.finish(None).await;
            }
            return cache_hit_response(
                &span,
                &state.sampling_gate,
                state.prompt_cache.kind(),
                ctx,
                &value,
            );
        }
    }

    let request_value = serde_json::from_slice::<serde_json::Value>(&body).unwrap_or_default();
    let (admission_input, soft_cap) = messages_admission_input(&request_value);
    let normalized_usage_input = admission_input.filter(|_| !soft_cap);
    let selection = match crate::judge::routing::select(
        state,
        api_key,
        public_model,
        &Default::default(),
        route,
        || crate::judge::input::JudgeInput::messages(public_model, &request_value),
        &span,
    )
    .await
    {
        Ok(selection) => selection,
        Err(error) => return crate::handlers::backend_error_response(&error),
    };
    let selected_model = selection.plan.primary_model.clone();
    let request = AnthropicMessagesRequest {
        body,
        headers: AnthropicRequestHeaders::from_header_map(headers),
        request_value,
        admission_input,
        soft_cap,
    };

    // Holds the most recently attempted native/adapted candidate's raw upstream
    // response whenever that attempt is converted into a retry-triggering
    // error (see `handle_raw_upstream_dispatch`). `try_with_fallbacks_if` tries
    // candidates strictly in order and only advances past a failure, so by the
    // time it returns `Err` this slot — written or cleared on every dispatched
    // attempt — always reflects (only) the last one actually tried, letting us
    // forward its bytes verbatim instead of a synthesized error below.
    let raw_failure = std::sync::Mutex::new(None::<RawUpstreamFailure>);
    let result = try_with_fallbacks_if(
        &span,
        state,
        request,
        selection.plan,
        api_key,
        admission_input.map(|input| input.conservative_tokens()),
        |route, request| {
            request
                .capability_eligible(route)
                .then_some(())
                .ok_or_else(|| {
                    "no configured provider can represent this request losslessly".to_string()
                })
        },
        |route, request| {
            let backend = route.backend.clone();
            let mode = anthropic_messages_mode(route);
            dispatch_anthropic_messages(backend, mode, request, &raw_failure)
        },
    )
    .await;

    let (outcome, fallback_model, resolution, mut rate_limit_permit) = match result {
        Ok(result) => result,
        Err(error) => {
            return match raw_failure.into_inner().ok().flatten() {
                Some(raw) => raw_upstream_failure_response(raw),
                None => crate::handlers::backend_error_response_for_format(
                    &error,
                    crate::backend::provider::UpstreamErrorFormat::Other,
                ),
            };
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

    let mut response = match outcome {
        AnthropicDispatchOutcome::Translated(translated) => {
            set_anthropic_output_span_attributes(&span, &translated.value);
            span.apply_outcome_sampling(&state.sampling_gate, &translated.value);
            let _token_usage = match record_anthropic_usage(
                state,
                api_key,
                &resolution,
                &span,
                &translated.value,
                normalized_usage_input,
                rate_limit_permit.take(),
            )
            .await
            {
                Ok(usage) => usage,
                Err(_) => return super::usage_write_error_response(),
            };
            let content_type = if translated.streaming {
                "text/event-stream"
            } else {
                "application/json"
            };
            let mut response = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, content_type)
                .body(Body::from(translated.body))
                .unwrap();
            crate::handlers::stamp_trace_headers(response.headers_mut());
            response
        }
        AnthropicDispatchOutcome::Native(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let resp_headers = resp.headers().clone();

            let content_type = resp_headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json")
                .to_string();

            let is_sse = content_type.contains("text/event-stream");

            if !status.is_success() {
                if let Some(permit) = rate_limit_permit.take() {
                    permit.finish_failed().await;
                }
            }

            if is_sse {
                tracing::debug!("Streaming Anthropic SSE response");

                // Tap the passthrough bytes: keeps the span's recorded end
                // time advancing while the body streams (see
                // `AnthropicSseSpanTap`'s doc comment) and assembles the SSE
                // deltas into output-message attributes. It also doubles as
                // the cache writer — see `proxy_request`'s equivalent block.
                let body = Body::from_stream(AnthropicSseSpanTap::new(
                    resp.bytes_stream(),
                    span.clone(),
                    start,
                    state.clone(),
                    api_key,
                    &resolution,
                    normalized_usage_input,
                    Some(resolution.upstream_model_id.clone()),
                    status.is_success().then(|| cache_ctx.clone()).flatten(),
                    rate_limit_permit.take(),
                ));

                let mut builder = Response::builder()
                    .status(status)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache");

                // Forward rate-limit / retry signaling (and request-id) so
                // clients can back off on a 429 just like on the OpenAI routes.
                for (name, value) in forwardable_upstream_headers(&resp_headers) {
                    builder = builder.header(name, value);
                }

                let mut res = builder.body(body).unwrap();
                crate::handlers::stamp_trace_headers(res.headers_mut());
                if let Some(ctx) = &cache_ctx {
                    crate::handlers::stamp_cache_outcome(
                        &span,
                        Some(res.headers_mut()),
                        state.prompt_cache.kind(),
                        ctx,
                        None,
                    );
                }
                res
            } else {
                let bytes = resp.bytes().await.unwrap_or_default();

                if !status.is_success() {
                    tracing::warn!(
                        status = %status,
                        body = %String::from_utf8_lossy(&bytes),
                        "Anthropic API error"
                    );
                } else {
                    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                        set_anthropic_output_span_attributes(&span, &value);
                        span.apply_outcome_sampling(&state.sampling_gate, &value);
                        let _token_usage = match record_anthropic_usage(
                            state,
                            api_key,
                            &resolution,
                            &span,
                            &value,
                            normalized_usage_input,
                            rate_limit_permit.take(),
                        )
                        .await
                        {
                            Ok(usage) => usage,
                            Err(_) => return super::usage_write_error_response(),
                        };
                    }
                    // Cache the upstream body verbatim, gated on success only
                    // (an error body must never be replayed as an answer).
                    if let Some(ctx) = &cache_ctx {
                        crate::handlers::store_completion(
                            state,
                            ctx,
                            bytes.to_vec(),
                            PayloadKind::Json,
                            public_model.to_string(),
                        );
                    }
                }

                let mut builder = Response::builder()
                    .status(status)
                    .header(header::CONTENT_TYPE, &content_type);

                for (name, value) in forwardable_upstream_headers(&resp_headers) {
                    builder = builder.header(name, value);
                }

                let mut res = builder.body(Body::from(bytes)).unwrap();
                crate::handlers::stamp_trace_headers(res.headers_mut());
                if let Some(ctx) = &cache_ctx {
                    crate::handlers::stamp_cache_outcome(
                        &span,
                        Some(res.headers_mut()),
                        state.prompt_cache.kind(),
                        ctx,
                        None,
                    );
                }
                res
            }
        }
    };
    stamp_routing_headers(
        response.headers_mut(),
        public_model,
        served_model,
        fallback_model.as_deref(),
    );
    response
}

pub async fn proxy_messages_count_tokens(
    State(state): State<crate::AppState>,
    api_key: ResolvedApiKey,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    proxy_request(
        &state,
        &api_key,
        AnthropicRequestEndpoint::CountTokens,
        &headers,
        body,
        None,
    )
    .await
}

/// Forward a request to Anthropic. Shared by `/v1/messages` and
/// `/v1/messages/count_tokens`; `llm` carries the typed span and start time for the
/// former (enabling OpenInference span attribution and the SSE tap) and `None`
/// for the latter, which stays span-free. Both resolve the public model once and
/// replace only its `model` field with the provider-native id before dispatch.
async fn proxy_request(
    state: &crate::AppState,
    api_key: &ResolvedApiKey,
    endpoint: AnthropicRequestEndpoint,
    headers: &HeaderMap,
    body: Bytes,
    llm: Option<(LlmSpanWriter, std::time::Instant)>,
) -> Response {
    let path = match endpoint {
        AnthropicRequestEndpoint::Messages => "/v1/messages",
        AnthropicRequestEndpoint::CountTokens => "/v1/messages/count_tokens",
    };
    tracing::debug!(path = %path, "Proxying Anthropic API request");

    let req_model = request_model(&body);
    let Some(public_model) = req_model.as_deref() else {
        return (
            StatusCode::BAD_REQUEST,
            "Anthropic request requires a string `model` field",
        )
            .into_response();
    };

    let route = match state.backend.resolve_anthropic(public_model).await {
        Ok(route) => route,
        Err(error) => {
            return crate::handlers::backend_error_response_for_format(
                &error,
                crate::backend::provider::UpstreamErrorFormat::Other,
            )
        }
    };

    // Stamp request attrs NOW — before the permission gate and dispatch — so a
    // denied/errored span still carries system/model/connection/input, mirroring
    // chat.rs's `set_route_span_attributes` / `set_input_span_attributes` ordering.
    if let Some((span, _)) = &llm {
        set_anthropic_request_span_attributes(span, &body, &route.resolution);
    }

    // Enforce the key's allowlist against the resolved Anthropic connection
    // (provider kind + immutable connection id + requested model) before
    // forwarding — the auth middleware only authenticated the key.
    if let Err(denied) = check_permission(
        &api_key.0.permission,
        &route.resolution,
        req_model.as_deref(),
    ) {
        tracing::warn!(
            key_prefix = %api_key.0.key_prefix,
            path = %path,
            reason = %denied,
            "Anthropic passthrough denied by API key permission"
        );
        return crate::handlers::forbidden_response(denied.to_string());
    }

    // Exact-prefix cache, consulted after the permission gate for the same
    // reason as on the chat surface: a hit must never be a way past the key's
    // allowlist. Keyed on the *client's* body — before the upstream model
    // rewrite below — so the digest names the requested public model id, which
    // is what the module's scoping guarantees are stated in terms of. Scoped to
    // the LLM path: `count_tokens` (`llm: None`) is never cached.
    //
    // Unlike `chat_cache_context` (one entry shared regardless of `stream`),
    // this keys on the raw request body, so `stream: true` and `stream: false`
    // requests for the same conversation land in separate cache entries. That
    // is intentional, not an oversight: this surface stores the *upstream
    // bytes* it replays on a hit (SSE frames for a streaming request, a single
    // JSON object otherwise), and those two byte streams are not
    // interchangeable, so they cannot share one entry.
    let cache_ctx = llm.as_ref().and_then(|_| {
        state.raw_body_cache_context(&api_key.0, Surface::AnthropicMessages, headers, &body)
    });
    if let (Some(ctx), Some((span, _))) = (&cache_ctx, &llm) {
        if let Some(value) = state.prompt_cache.get(&ctx.key).await {
            let cache_permit = match acquire_request_quota(
                state,
                api_key,
                TokenReservation::NotApplicable,
            )
            .await
            {
                Ok(permit) => permit,
                Err(error) => {
                    return crate::handlers::backend_error_response_for_format(
                        &error,
                        crate::backend::provider::UpstreamErrorFormat::Other,
                    )
                }
            };
            if let Some(permit) = cache_permit {
                permit.finish(None).await;
            }
            return cache_hit_response(
                span,
                &state.sampling_gate,
                state.prompt_cache.kind(),
                ctx,
                &value,
            );
        }
    }

    let upstream_body = match rewrite_request_model(&body, &route.stripped_model) {
        Ok(body) => body,
        Err(error) => return (StatusCode::BAD_REQUEST, error).into_response(),
    };
    let upstream_headers = AnthropicRequestHeaders::from_header_map(headers);

    let operation = match endpoint {
        AnthropicRequestEndpoint::Messages => ProtocolOperation::Create,
        AnthropicRequestEndpoint::CountTokens => ProtocolOperation::CountTokens,
    };
    let mode = route
        .capabilities
        .profile
        .operation(Protocol::AnthropicMessages, operation)
        .map_or(DeliveryMode::Unknown, |profile| profile.mode);

    let request_value = serde_json::from_slice::<serde_json::Value>(&body).unwrap_or_default();
    let (admission_input, soft_cap) = messages_admission_input(&request_value);
    let normalized_usage_input = admission_input.filter(|_| !soft_cap);
    let requirements = anthropic_requirements(endpoint, &request_value);
    crate::capabilities::trace_decision(
        &route,
        public_model,
        &requirements,
        match endpoint {
            AnthropicRequestEndpoint::Messages => "anthropic_messages.create",
            AnthropicRequestEndpoint::CountTokens => "anthropic_messages.count_tokens",
        },
    );
    if !route.capabilities.profile.satisfies(&requirements) {
        return crate::handlers::backend_error_response(&BackendError::api_status(
            400,
            format!(
                "Anthropic {} is explicitly unsupported for model '{public_model}'",
                match endpoint {
                    AnthropicRequestEndpoint::Messages => "Messages request",
                    AnthropicRequestEndpoint::CountTokens => "count_tokens",
                }
            ),
        ));
    }

    let input_tokens = admission_input.map(|input| input.conservative_tokens());
    let requested_output = request_value
        .get("max_tokens")
        .and_then(serde_json::Value::as_u64);
    let output_tokens = requested_output.or(route.capabilities.profile.limits.max_output_tokens);
    let token_reservation = input_tokens
        .zip(output_tokens)
        .map(|(input, output)| input.saturating_add(output));
    let mut rate_limit_permit = if endpoint == AnthropicRequestEndpoint::Messages {
        let reservation = TokenReservation::from_option(token_reservation).with_soft_cap(soft_cap);
        let request_permit = match acquire_request_quota(state, api_key, reservation).await {
            Ok(permit) => permit,
            Err(error) => {
                return crate::handlers::backend_error_response_for_format(
                    &error,
                    crate::backend::provider::UpstreamErrorFormat::Other,
                )
            }
        };
        let rate_permit = match acquire_rate_limit(
            state,
            api_key,
            &route.resolution,
            RateLimitModel::Exact(public_model),
            reservation,
            input_tokens,
        )
        .await
        {
            Ok(permit) => permit,
            Err(error) => {
                if let Some(permit) = request_permit {
                    permit.refund().await;
                }
                return crate::handlers::backend_error_response_for_format(
                    &error,
                    crate::backend::provider::UpstreamErrorFormat::Other,
                );
            }
        };
        QuotaPermit::merge(request_permit, rate_permit)
    } else {
        match acquire_single_route_quota(
            state,
            api_key,
            &route.resolution,
            RateLimitModel::Exact(public_model),
            TokenReservation::NotApplicable,
            None,
        )
        .await
        {
            Ok(permit) => permit,
            Err(error) => {
                return crate::handlers::backend_error_response_for_format(
                    &error,
                    crate::backend::provider::UpstreamErrorFormat::Other,
                )
            }
        }
    };

    if mode == DeliveryMode::Translated
        || (mode == DeliveryMode::Unknown && endpoint == AnthropicRequestEndpoint::Messages)
    {
        if endpoint == AnthropicRequestEndpoint::CountTokens {
            return crate::handlers::backend_error_response(&BackendError::api_status(
                400,
                "Anthropic count_tokens has no lossless Chat translation",
            ));
        }
        return match crate::adapters::anthropic_chat::translate_messages(
            route.backend.as_ref(),
            &upstream_body,
        )
        .await
        {
            Ok(translated) => {
                if let Some((span, _)) = &llm {
                    set_anthropic_output_span_attributes(span, &translated.value);
                    let _token_usage = match record_anthropic_usage(
                        state,
                        api_key,
                        &route.resolution,
                        span,
                        &translated.value,
                        normalized_usage_input,
                        rate_limit_permit.take(),
                    )
                    .await
                    {
                        Ok(usage) => usage,
                        Err(_) => return super::usage_write_error_response(),
                    };
                }
                let content_type = if translated.streaming {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                let mut response = Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, content_type)
                    .body(Body::from(translated.body))
                    .unwrap();
                if llm.is_some() {
                    crate::handlers::stamp_trace_headers(response.headers_mut());
                }
                response
            }
            Err(error) => {
                if let Some(permit) = rate_limit_permit.take() {
                    permit.finish_failed().await;
                }
                crate::handlers::backend_error_response_for_format(
                    &error,
                    crate::backend::provider::UpstreamErrorFormat::Other,
                )
            }
        };
    }

    if mode == DeliveryMode::Adapted && endpoint == AnthropicRequestEndpoint::CountTokens {
        return match route.backend.count_anthropic_tokens(upstream_body).await {
            Ok(upstream) => {
                if let Some(permit) = rate_limit_permit.take() {
                    permit.finish(None).await;
                }
                let mut response = axum::Json(upstream.body).into_response();
                crate::handlers::apply_forwarded_headers(
                    response.headers_mut(),
                    &upstream.response.headers,
                );
                response
            }
            Err(error) => {
                if let Some(permit) = rate_limit_permit.take() {
                    permit.finish_failed().await;
                }
                crate::handlers::backend_error_response_for_format(
                    &error,
                    crate::backend::provider::UpstreamErrorFormat::Other,
                )
            }
        };
    }

    let native_response = match mode {
        DeliveryMode::Native | DeliveryMode::Unknown => {
            route
                .backend
                .send_anthropic_request(endpoint, &upstream_headers, upstream_body)
                .await
        }
        DeliveryMode::Adapted => {
            route
                .backend
                .send_anthropic_adapted_request(endpoint, &upstream_headers, upstream_body)
                .await
        }
        DeliveryMode::Unsupported => {
            return crate::handlers::backend_error_response(&BackendError::api_status(
                400,
                format!(
                    "Anthropic {} is {mode:?} for model '{public_model}'",
                    match endpoint {
                        AnthropicRequestEndpoint::Messages => "Messages",
                        AnthropicRequestEndpoint::CountTokens => "count_tokens",
                    }
                ),
            ));
        }
        DeliveryMode::Translated => unreachable!("translated handled above"),
    };

    match native_response {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let resp_headers = resp.headers().clone();

            let content_type = resp_headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json")
                .to_string();

            let is_sse = content_type.contains("text/event-stream");

            if !status.is_success() {
                if let Some(permit) = rate_limit_permit.take() {
                    permit.finish_failed().await;
                }
            }

            if is_sse {
                tracing::debug!("Streaming Anthropic SSE response");

                // Tap the passthrough bytes on the LLM-instrumented path: keeps
                // the span's recorded end time advancing while the body
                // streams (see `AnthropicSseSpanTap`'s doc comment) and
                // assembles the SSE deltas into output-message attributes.
                // `count_tokens` never reaches here (it isn't SSE), but the
                // `llm` gate keeps this generic against that possibility.
                let body = match &llm {
                    Some((span, start)) => Body::from_stream(AnthropicSseSpanTap::new(
                        resp.bytes_stream(),
                        span.clone(),
                        *start,
                        state.clone(),
                        api_key,
                        &route.resolution,
                        normalized_usage_input,
                        Some(route.stripped_model.clone()),
                        // The tap doubles as the cache writer: it already sees
                        // every passthrough byte, so it buffers the stream and
                        // stores it whole once the upstream finishes (see
                        // `AnthropicSseSpanTap::finalize`). Only successful
                        // responses are cached — an upstream error's SSE body
                        // must not be replayed as an answer.
                        status.is_success().then(|| cache_ctx.clone()).flatten(),
                        rate_limit_permit.take(),
                    )),
                    None => Body::from_stream(resp.bytes_stream()),
                };

                let mut builder = Response::builder()
                    .status(status)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache");

                // Forward rate-limit / retry signaling (and request-id) so
                // clients can back off on a 429 just like on the OpenAI routes.
                for (name, value) in forwardable_upstream_headers(&resp_headers) {
                    builder = builder.header(name, value);
                }

                let mut res = builder.body(body).unwrap();
                if llm.is_some() {
                    crate::handlers::stamp_trace_headers(res.headers_mut());
                }
                if let (Some(ctx), Some((span, _))) = (&cache_ctx, &llm) {
                    crate::handlers::stamp_cache_outcome(
                        span,
                        Some(res.headers_mut()),
                        state.prompt_cache.kind(),
                        ctx,
                        None,
                    );
                }
                res
            } else {
                let bytes = resp.bytes().await.unwrap_or_default();

                if !status.is_success() {
                    tracing::warn!(
                        status = %status,
                        body = %String::from_utf8_lossy(&bytes),
                        "Anthropic API error"
                    );
                } else if let Some((span, _)) = &llm {
                    // OpenInference output attrs: only for a successful
                    // Messages response, on the LLM-instrumented path.
                    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                        set_anthropic_output_span_attributes(span, &value);
                        let _token_usage = match record_anthropic_usage(
                            state,
                            api_key,
                            &route.resolution,
                            span,
                            &value,
                            normalized_usage_input,
                            rate_limit_permit.take(),
                        )
                        .await
                        {
                            Ok(usage) => usage,
                            Err(_) => return super::usage_write_error_response(),
                        };
                    }
                    // Cache the upstream body verbatim. Gated on success only —
                    // an error body must never be replayed as an answer — and
                    // written outside the JSON parse above, since what is stored
                    // is the bytes, not our reading of them.
                    if let Some(ctx) = &cache_ctx {
                        crate::handlers::store_completion(
                            state,
                            ctx,
                            bytes.to_vec(),
                            PayloadKind::Json,
                            req_model.clone().unwrap_or_default(),
                        );
                    }
                } else if let Some(permit) = rate_limit_permit.take() {
                    permit.finish(None).await;
                }

                let mut builder = Response::builder()
                    .status(status)
                    .header(header::CONTENT_TYPE, &content_type);

                // Forward rate-limit / retry signaling (and request-id) so
                // clients can back off on a 429 just like on the OpenAI routes.
                for (name, value) in forwardable_upstream_headers(&resp_headers) {
                    builder = builder.header(name, value);
                }

                let mut res = builder.body(Body::from(bytes)).unwrap();
                if llm.is_some() {
                    crate::handlers::stamp_trace_headers(res.headers_mut());
                }
                if let (Some(ctx), Some((span, _))) = (&cache_ctx, &llm) {
                    crate::handlers::stamp_cache_outcome(
                        span,
                        Some(res.headers_mut()),
                        state.prompt_cache.kind(),
                        ctx,
                        None,
                    );
                }
                res
            }
        }
        Err(BackendError::RequestError(error)) => {
            if let Some(permit) = rate_limit_permit.take() {
                permit.finish_failed().await;
            }
            tracing::error!(%error, "Anthropic proxy error");
            (StatusCode::BAD_GATEWAY, format!("Proxy error: {error}")).into_response()
        }
        Err(error) => {
            if let Some(permit) = rate_limit_permit.take() {
                permit.finish_failed().await;
            }
            tracing::error!(%error, "Anthropic proxy error");
            crate::handlers::backend_error_response_for_format(
                &error,
                crate::backend::provider::UpstreamErrorFormat::Other,
            )
        }
    }
}

fn anthropic_requirements(
    endpoint: AnthropicRequestEndpoint,
    request: &serde_json::Value,
) -> CapabilityRequirements {
    let operation = match endpoint {
        AnthropicRequestEndpoint::Messages => ProtocolOperation::Create,
        AnthropicRequestEndpoint::CountTokens => ProtocolOperation::CountTokens,
    };
    let mut requirements =
        CapabilityRequirements::for_operation(Protocol::AnthropicMessages, operation)
            .surface(ModelSurface::Chat);
    let scan = scan_anthropic_request(request);
    if request.get("system").is_some()
        || scan.has_text_or_tool_result
        || request
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|messages| {
                messages.iter().any(|message| {
                    message
                        .get("content")
                        .is_some_and(serde_json::Value::is_string)
                })
            })
    {
        requirements = requirements.input(InputModality::Text);
    }
    if endpoint == AnthropicRequestEndpoint::Messages {
        requirements = requirements.output(OutputModality::Text);
    }
    if request.get("stream").and_then(serde_json::Value::as_bool) == Some(true) {
        requirements = requirements.endpoint_feature(EndpointFeature::Streaming);
    }
    if request
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|tools| !tools.is_empty())
        || scan.has_tool_use_or_tool_result
    {
        requirements = requirements
            .feature(ModelFeature::ToolCalling)
            .endpoint_feature(EndpointFeature::ToolCalling);
    }
    if scan.has_image {
        requirements = requirements
            .input(InputModality::Image)
            .endpoint_feature(EndpointFeature::ImageInput);
    }
    if scan.has_cache_control {
        requirements = requirements.feature(ModelFeature::PromptCaching);
    }
    requirements
}

/// Every capability signal [`anthropic_requirements`] needs from the JSON
/// body, gathered in one recursive descent instead of four separate walks
/// (three re-walking `messages` for different content-block `"type"` values,
/// one re-walking the whole request for a `cache_control` key).
#[derive(Default)]
struct AnthropicRequestScan {
    /// A `messages` content block is `"text"` or `"tool_result"` typed.
    has_text_or_tool_result: bool,
    /// A `messages` content block is `"tool_use"` or `"tool_result"` typed.
    has_tool_use_or_tool_result: bool,
    /// A `messages` content block is `"image"` typed.
    has_image: bool,
    /// A `cache_control` key appears anywhere in the request, not just inside
    /// `messages` (it's also legal on top-level `system` and `tools` blocks).
    has_cache_control: bool,
}

fn scan_anthropic_request(request: &serde_json::Value) -> AnthropicRequestScan {
    let mut scan = AnthropicRequestScan::default();
    scan_anthropic_value(request, false, &mut scan);
    scan
}

fn scan_anthropic_value(
    value: &serde_json::Value,
    in_messages: bool,
    scan: &mut AnthropicRequestScan,
) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                scan_anthropic_value(value, in_messages, scan);
            }
        }
        serde_json::Value::Object(fields) => {
            if fields.contains_key("cache_control") {
                scan.has_cache_control = true;
            }
            if in_messages {
                if let Some(kind) = fields.get("type").and_then(serde_json::Value::as_str) {
                    if matches!(kind, "text" | "tool_result") {
                        scan.has_text_or_tool_result = true;
                    }
                    if matches!(kind, "tool_use" | "tool_result") {
                        scan.has_tool_use_or_tool_result = true;
                    }
                    if kind == "image" {
                        scan.has_image = true;
                    }
                }
            }
            for (key, value) in fields {
                scan_anthropic_value(value, in_messages || key == "messages", scan);
            }
        }
        _ => {}
    }
}

/// Accumulated state for one Anthropic `tool_use` content block, keyed by its
/// SSE `content_block_start`/`content_block_delta` `index`.
#[derive(Default)]
struct AnthropicToolCallAccumulator {
    id: String,
    name: String,
    /// Concatenated `input_json_delta.partial_json` fragments — valid JSON
    /// only once complete, so it's kept as a string until finalize.
    arguments: String,
}

/// Apply the output tracing and outcome policy consistently to cached JSON,
/// live SSE, and replayed cached SSE responses.
fn stamp_anthropic_response(
    span: &LlmSpanWriter,
    sampling_gate: &std::sync::Arc<crate::telemetry::sampling::SamplingGate>,
    response: &serde_json::Value,
) {
    set_anthropic_output_span_attributes(span, response);
    span.apply_outcome_sampling(sampling_gate, response);
}

struct AnthropicStreamResponse<'a> {
    role: Option<&'a str>,
    text: &'a str,
    tool_calls: &'a BTreeMap<i64, AnthropicToolCallAccumulator>,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    cache_creation_tokens: u64,
    stop_reason: Option<&'a str>,
}

fn assembled_anthropic_stream_response(
    stream: AnthropicStreamResponse<'_>,
) -> Option<serde_json::Value> {
    if stream.role.is_none() && stream.text.is_empty() && stream.tool_calls.is_empty() {
        return None;
    }

    let mut content = Vec::new();
    if !stream.text.is_empty() {
        content.push(serde_json::json!({"type": "text", "text": stream.text}));
    }
    for acc in stream.tool_calls.values() {
        let input = serde_json::from_str::<serde_json::Value>(&acc.arguments)
            .unwrap_or_else(|_| serde_json::Value::String(acc.arguments.clone()));
        content.push(serde_json::json!({
            "type": "tool_use",
            "id": acc.id,
            "name": acc.name,
            "input": input,
        }));
    }

    Some(serde_json::json!({
        "role": stream.role.unwrap_or("assistant"),
        "content": content,
        "stop_reason": stream.stop_reason,
        "usage": {
            "input_tokens": stream.input_tokens,
            "output_tokens": stream.output_tokens,
            "cache_read_input_tokens": stream.cached_tokens,
            "cache_creation_input_tokens": stream.cache_creation_tokens,
        },
    }))
}

/// Reconstruct the outcome-rule response shape from a cached SSE payload.
/// This owns no request state and performs no usage or cache writes, which
/// lets cache hits obtain the same outcome decision as a live stream while
/// returning the stored bytes unchanged.
fn anthropic_response_from_cached_sse(payload: &[u8]) -> Option<serde_json::Value> {
    #[derive(Default)]
    struct Accumulator {
        role: Option<String>,
        text: String,
        tool_calls: BTreeMap<i64, AnthropicToolCallAccumulator>,
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
        cache_creation_tokens: u64,
        stop_reason: Option<String>,
    }

    let mut decoder = SseDecoder::default();
    let mut frames = decoder.push(payload);
    frames.extend(decoder.finish());
    let mut acc = Accumulator::default();
    for frame in frames {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&frame.data) else {
            continue;
        };
        match value.get("type").and_then(|value| value.as_str()) {
            Some("message_start") => {
                let Some(message) = value.get("message") else {
                    continue;
                };
                acc.role = message
                    .get("role")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned);
                if let Some(usage) = message.get("usage") {
                    let tokens = anthropic_usage_tokens(usage);
                    acc.input_tokens = tokens.input_tokens;
                    acc.cached_tokens = tokens.cache_read_input_tokens;
                    acc.cache_creation_tokens = tokens.cache_creation_input_tokens;
                }
            }
            Some("content_block_start") => {
                let index = value
                    .get("index")
                    .and_then(|value| value.as_i64())
                    .unwrap_or(0);
                let Some(block) = value.get("content_block") else {
                    continue;
                };
                if block.get("type").and_then(|value| value.as_str()) != Some("tool_use") {
                    continue;
                }
                let tool = acc.tool_calls.entry(index).or_default();
                tool.id = block
                    .get("id")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_owned();
                tool.name = block
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_owned();
            }
            Some("content_block_delta") => {
                let index = value
                    .get("index")
                    .and_then(|value| value.as_i64())
                    .unwrap_or(0);
                let Some(delta) = value.get("delta") else {
                    continue;
                };
                match delta.get("type").and_then(|value| value.as_str()) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(|value| value.as_str()) {
                            acc.text.push_str(text);
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(partial) =
                            delta.get("partial_json").and_then(|value| value.as_str())
                        {
                            acc.tool_calls
                                .entry(index)
                                .or_default()
                                .arguments
                                .push_str(partial);
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(usage) = value.get("usage") {
                    if usage.get("output_tokens").is_some() {
                        acc.output_tokens = anthropic_usage_tokens(usage).output_tokens;
                    }
                }
                acc.stop_reason = value
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(|value| value.as_str())
                    .map(str::to_owned);
            }
            _ => {}
        }
    }
    assembled_anthropic_stream_response(AnthropicStreamResponse {
        role: acc.role.as_deref(),
        text: &acc.text,
        tool_calls: &acc.tool_calls,
        input_tokens: acc.input_tokens,
        output_tokens: acc.output_tokens,
        cached_tokens: acc.cached_tokens,
        cache_creation_tokens: acc.cache_creation_tokens,
        stop_reason: acc.stop_reason.as_deref(),
    })
}

/// Passthrough tap over the upstream Anthropic SSE byte stream that carries the
/// `anthropic.messages` span for the life of the stream.
///
/// The handler future returns the `Response` — and exits the span for the last
/// time — as soon as the 200/SSE response is handed back, long before the
/// model finishes generating. tracing-opentelemetry stamps the exported span
/// end time on every span *exit*, so unless something re-enters the span while
/// the body streams, the exported span records only the time-to-open-stream
/// (milliseconds) instead of the full generation. Entering the span on every
/// body poll keeps the recorded end time advancing until the stream completes
/// (mirrors `SseSpanTap` in chat.rs).
///
/// The item type is `Result<Bytes, reqwest::Error>`, matching
/// `reqwest::Response::bytes_stream()` exactly: both `Ok` and `Err` items are
/// forwarded unchanged (a transparent passthrough — unlike the OpenAI path,
/// this does NOT translate a stream error into an in-band SSE event).
///
/// While chunks pass through, the tap also accumulates the streamed
/// `message_start` / `content_block_start` / `content_block_delta` /
/// `message_delta` events into an Anthropic-response-shaped JSON value and, at
/// `message_stop` (or stream end, or drop on client disconnect), stamps it
/// onto the span via [`set_anthropic_output_span_attributes`] — the same
/// function the non-streaming path uses on the real response body.
///
/// SSE events may split across network chunks, so frames are decoded
/// incrementally via `SseDecoder` rather than parsed off of each raw chunk.
/// `event: ...` lines are ignored; only the frame's `data` is parsed, and
/// dispatch is on the JSON payload's own `type` field.
struct AnthropicSseSpanTap {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    span: LlmSpanWriter,
    /// Decodes complete SSE frames from the upstream byte stream, so a
    /// multibyte character or JSON payload split across two network chunks is
    /// never misparsed (see `process_frame`).
    decoder: SseDecoder,
    finalized: bool,
    /// Request-handling start, for `llm.time_to_first_token_ms`.
    start: std::time::Instant,
    /// Set on the first content-bearing delta (text or tool-call input json),
    /// so the time-to-first-token attribute is stamped exactly once and never
    /// on `content_block_start` (which announces a block with no content yet)
    /// or an empty delta fragment.
    first_token_seen: bool,
    /// Cached once at construction (backed by a process-lifetime `OnceLock`,
    /// so a single read is sound) rather than re-checked per delta: gates
    /// accumulating text / tool-call-argument bytes, which in metadata-only
    /// mode would otherwise buffer the full generation in memory per
    /// concurrent stream for attributes that are never stamped. Also forced
    /// on when the active policy has an outcome rule to evaluate —
    /// `set_anthropic_output_span_attributes` independently re-derives the
    /// real content-logging setting before attaching anything to the span, so
    /// this never leaks content the operator disabled logging for.
    log_content: bool,
    role: Option<String>,
    /// Concatenated `text_delta.text` fragments across all text blocks (in
    /// practice there is exactly one text block per response). Left empty
    /// when `log_content` is false.
    text: String,
    tool_calls: BTreeMap<i64, AnthropicToolCallAccumulator>,
    input_tokens: u64,
    /// From `message_delta.usage.output_tokens`, cumulative — Anthropic
    /// resends the running total on each `message_delta`, so the latest value
    /// read wins.
    output_tokens: u64,
    /// From `message_start.message.usage.cache_read_input_tokens`; unlike
    /// `output_tokens` this isn't re-sent on `message_delta`, so it's set once.
    cached_tokens: u64,
    /// From `message_start.message.usage.cache_creation_input_tokens`; like
    /// `cached_tokens`, set once. Anthropic's `input_tokens` excludes both
    /// cache fields, so the true prompt total sums all three (see
    /// `AnthropicUsageTokens::prompt_tokens`).
    cache_creation_tokens: u64,
    input_usage_observed: bool,
    output_usage_observed: bool,
    estimate: Option<UsageEstimateState>,
    stop_reason: Option<String>,
    /// Usage-recording context, mirroring `SseSpanTap` in chat.rs: cloned
    /// `AppState` (for the store/tracker handles) plus the presenting key's
    /// identity. `model` starts from the resolved request and can be filled
    /// from `message_start.message.model` if necessary.
    state: crate::AppState,
    usage_attribution: UsageAttribution,
    model: Option<String>,
    pending_write: Option<UsageWriteFuture>,
    rate_limit_permit: Option<QuotaPermit>,
    pending_terminal: Option<Bytes>,
    end_after_write: bool,
    terminated: bool,
    /// Cache slot for this request, when the key opted in and the upstream
    /// returned success. `None` disables all of the buffering below.
    cache_ctx: Option<CacheContext>,
    /// Every passthrough byte, buffered so the finished stream can be stored
    /// and replayed verbatim. Only allocated when `cache_ctx` is set.
    cache_buf: Vec<u8>,
    /// Set once the stream exceeds the entry ceiling. Buffering then stops and
    /// the entry is abandoned: a cache is not worth holding an unbounded
    /// generation in memory per concurrent stream for.
    cache_overflowed: bool,
    /// Whether the upstream stream ended in a state that means "this response
    /// is whole" — a clean end of stream. A transport error or a client
    /// disconnect leaves it false, and only a whole response is ever cached.
    clean_end: bool,
}

impl AnthropicSseSpanTap {
    #[allow(clippy::too_many_arguments)]
    fn new(
        inner: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
        span: LlmSpanWriter,
        start: std::time::Instant,
        state: crate::AppState,
        api_key: &ResolvedApiKey,
        resolution: &RouteResolution,
        normalized_usage_input: Option<NormalizedUsageInput>,
        model: Option<String>,
        cache_ctx: Option<CacheContext>,
        rate_limit_permit: Option<QuotaPermit>,
    ) -> Self {
        // Read once, like `content_enabled()` alone used to be: a policy edit
        // that adds an outcome rule mid-stream does not retroactively turn on
        // accumulation for a request already in flight — the same accepted
        // staleness window the content-logging flag already had.
        let log_content = span.content_enabled() || state.sampling_gate.has_outcome_rules();
        Self {
            inner: Box::pin(inner),
            span,
            decoder: SseDecoder::default(),
            finalized: false,
            start,
            first_token_seen: false,
            log_content,
            role: None,
            text: String::new(),
            tool_calls: BTreeMap::new(),
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            cache_creation_tokens: 0,
            input_usage_observed: false,
            output_usage_observed: false,
            estimate: UsageEstimateState::new(resolution, normalized_usage_input),
            stop_reason: None,
            usage_attribution: UsageAttribution::new(api_key, resolution),
            model,
            state,
            pending_write: None,
            rate_limit_permit,
            pending_terminal: None,
            end_after_write: false,
            terminated: false,
            cache_ctx,
            cache_buf: Vec::new(),
            cache_overflowed: false,
            clean_end: false,
        }
    }

    /// Buffer one passthrough chunk toward the cached entry.
    ///
    /// Stops (and abandons the entry) at the ceiling rather than truncating:
    /// a partial SSE stream replayed to a later caller would look like a
    /// complete but silently shortened answer.
    fn buffer_for_cache(&mut self, bytes: &Bytes) {
        let Some(ctx) = self.cache_ctx.as_ref() else {
            return;
        };
        if self.cache_overflowed {
            return;
        }
        if self.cache_buf.len() + bytes.len() > ctx.max_entry_bytes {
            self.cache_overflowed = true;
            // Release what was buffered: the entry will not be written, so
            // holding it for the rest of the stream is pure waste.
            self.cache_buf = Vec::new();
            tracing::debug!(
                max_entry_bytes = ctx.max_entry_bytes,
                "prompt cache: streamed Messages response exceeds the entry ceiling; not cached"
            );
            return;
        }
        self.cache_buf.extend_from_slice(bytes);
    }

    /// Store the buffered stream, if it is cacheable and ended whole.
    ///
    /// `stop_reason` stands in for "the model finished": Anthropic sends it on
    /// `message_delta` before `message_stop`, so its absence means the
    /// generation was cut short even if the transport closed without an error.
    fn store_in_cache(&mut self) {
        let Some(ctx) = self.cache_ctx.as_ref() else {
            return;
        };
        if self.cache_overflowed || self.cache_buf.is_empty() {
            return;
        }
        if !self.clean_end {
            tracing::debug!("prompt cache: Messages stream did not end cleanly; not cached");
            return;
        }
        if self.stop_reason.is_none() {
            tracing::debug!("prompt cache: Messages stream carried no stop_reason; not cached");
            return;
        }
        crate::handlers::store_completion(
            &self.state,
            ctx,
            std::mem::take(&mut self.cache_buf),
            PayloadKind::Sse,
            self.model.clone().unwrap_or_default(),
        );
    }

    /// Consume the complete SSE lines now in the buffer, keeping a trailing
    /// partial line for the next chunk. Raw bytes are appended as-is and only
    /// a *complete* line is decoded (`from_utf8_lossy` on a whole line is
    /// fine; doing it per-chunk would split a multibyte character across two
    /// chunks into replacement characters).
    /// Decode the complete SSE frames now available and observe each in turn.
    fn process_chunk(&mut self, bytes: &Bytes) {
        for frame in self.decoder.push(bytes) {
            self.process_frame(frame);
        }
    }

    fn process_frame(&mut self, frame: SseFrame) {
        // `event: ...` lines are captured on `frame.event` but unused here;
        // dispatch is entirely on the JSON payload's own `type` field.
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&frame.data) else {
            return;
        };
        match value.get("type").and_then(|t| t.as_str()) {
            Some("message_start") => {
                let Some(message) = value.get("message") else {
                    return;
                };
                if let Some(role) = message.get("role").and_then(|r| r.as_str()) {
                    self.role = Some(role.to_string());
                }
                if self.model.is_none() {
                    if let Some(model) = message.get("model").and_then(|m| m.as_str()) {
                        self.model = Some(model.to_string());
                    }
                }
                if let Some(usage) = message.get("usage") {
                    let tokens = anthropic_usage_tokens(usage);
                    if usage
                        .get("input_tokens")
                        .and_then(|value| value.as_u64())
                        .is_some()
                    {
                        self.input_usage_observed = true;
                        self.input_tokens = tokens.input_tokens;
                        self.cached_tokens = tokens.cache_read_input_tokens;
                        self.cache_creation_tokens = tokens.cache_creation_input_tokens;
                    }
                }
            }
            Some("content_block_start") => {
                let index = value.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                let Some(block) = value.get("content_block") else {
                    return;
                };
                if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                    return;
                }
                let acc = self.tool_calls.entry(index).or_default();
                if let Some(id) = block.get("id").and_then(|i| i.as_str()) {
                    acc.id = id.to_string();
                }
                if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                    if let Some(estimate) = self.estimate.as_mut() {
                        estimate.observe_text(name);
                    }
                    acc.name = name.to_string();
                }
            }
            Some("content_block_delta") => {
                let index = value.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                let Some(delta) = value.get("delta") else {
                    return;
                };
                match delta.get("type").and_then(|t| t.as_str()) {
                    Some("text_delta") => {
                        // An empty fragment is a no-op: nothing to accumulate,
                        // and it must not stamp time-to-first-token (Anthropic
                        // can send a zero-length text_delta).
                        if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                            if !text.is_empty() {
                                self.stamp_time_to_first_token();
                                if let Some(estimate) = self.estimate.as_mut() {
                                    estimate.observe_text(text);
                                }
                                if self.log_content {
                                    self.text.push_str(text);
                                }
                            }
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(partial) = delta.get("partial_json").and_then(|p| p.as_str()) {
                            if !partial.is_empty() {
                                self.stamp_time_to_first_token();
                                if let Some(estimate) = self.estimate.as_mut() {
                                    estimate.observe_text(partial);
                                }
                                if self.log_content {
                                    self.tool_calls
                                        .entry(index)
                                        .or_default()
                                        .arguments
                                        .push_str(partial);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                // Only overwrite when the field is actually present — the
                // extractor defaults an absent `output_tokens` to 0, which
                // must not clobber a count from an earlier `message_delta`.
                if let Some(usage) = value.get("usage") {
                    if usage.get("output_tokens").is_some() {
                        self.output_usage_observed = true;
                        self.output_tokens = anthropic_usage_tokens(usage).output_tokens;
                    }
                }
                if let Some(stop_reason) = value
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|s| s.as_str())
                {
                    self.stop_reason = Some(stop_reason.to_string());
                }
            }
            Some("message_stop") => {
                // The upstream said the response is complete; mark it before
                // finalizing, since `finalize` runs exactly once and decides
                // there whether the buffered stream may be cached.
                self.clean_end = true;
                self.finalize()
            }
            _ => {}
        }
    }

    fn stamp_time_to_first_token(&mut self) {
        if self.first_token_seen {
            return;
        }
        self.first_token_seen = true;
        self.span
            .set_time_to_first_token_ms(self.start.elapsed().as_millis() as i64);
    }

    /// Finalize authoritative usage, or the conservative text estimate when
    /// either required cumulative count was absent.
    fn record_usage(&mut self) {
        let reported = (self.input_usage_observed && self.output_usage_observed).then(|| {
            let prompt_tokens = self
                .input_tokens
                .saturating_add(self.cached_tokens)
                .saturating_add(self.cache_creation_tokens);
            TokenUsage {
                input_tokens: prompt_tokens,
                output_tokens: self.output_tokens,
                output_tokens_observed: true,
                cached_input_tokens: self.cached_tokens,
                cache_write_input_tokens: self.cache_creation_tokens,
                reasoning_tokens: 0,
                total_tokens: prompt_tokens.saturating_add(self.output_tokens),
                usage_source: crate::usage_tracker::UsageSource::Reported,
                ..TokenUsage::default()
            }
        });
        let token_usage = reported.or_else(|| self.estimate.take().map(TokenUsage::from_estimate));
        if let Some(token_usage) = token_usage {
            token_usage.stamp(&self.span);
            let write = usage::record_future(
                self.state.clone(),
                self.usage_attribution.clone(),
                token_usage,
            );
            let actual_amount = self.clean_end.then_some(token_usage.total_tokens);
            self.pending_write = Some(usage::with_quota_settlement(
                write,
                self.rate_limit_permit.take(),
                actual_amount,
            ));
        } else {
            self.pending_write = Some(usage::with_quota_settlement(
                usage::missing_usage_future(),
                self.rate_limit_permit.take(),
                None,
            ));
        }
    }

    /// Stamp the assembled output onto the span. Runs once — at
    /// `message_stop`, stream end, or drop (client disconnect), whichever
    /// comes first — so a severed stream still records what was generated so
    /// far.
    fn finalize(&mut self) {
        if self.finalized {
            return;
        }
        self.finalized = true;

        // Record BEFORE the `saw_anything` gate below: usage tokens (from
        // `message_start`) can be present even when a stream is severed before
        // any content block starts, and a client disconnect should still
        // record what was generated so far.
        self.record_usage();

        let Some(response) = assembled_anthropic_stream_response(AnthropicStreamResponse {
            role: self.role.as_deref(),
            text: &self.text,
            tool_calls: &self.tool_calls,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_tokens: self.cached_tokens,
            cache_creation_tokens: self.cache_creation_tokens,
            stop_reason: self.stop_reason.as_deref(),
        }) else {
            return;
        };
        stamp_anthropic_response(&self.span, &self.state.sampling_gate, &response);
        self.store_in_cache();
    }

    fn poll_usage_write(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Option<Poll<Option<Result<Bytes, reqwest::Error>>>> {
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
                tracing::error!(error = %error, "Anthropic stream usage could not be persisted");
                self.pending_write = None;
                self.pending_terminal = None;
                self.terminated = true;
                self.span
                    .set_error("usage_accounting_unavailable", Some(&error.to_string()));
                let payload = serde_json::json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": "Usage accounting is temporarily unavailable"
                    }
                });
                Some(Poll::Ready(Some(Ok(encode_frame(
                    Some("error"),
                    &payload.to_string(),
                )))))
            }
        }
    }
}

impl Stream for AnthropicSseSpanTap {
    type Item = Result<Bytes, reqwest::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        // Enter the span for the duration of the poll so tracing-opentelemetry
        // re-stamps the exported end time (it does so on every exit); the last
        // poll therefore leaves the end time at stream completion.
        let span = this.span.clone();
        let _enter = span.enter();
        if this.terminated {
            return Poll::Ready(None);
        }
        if let Some(result) = this.poll_usage_write(cx) {
            return result;
        }
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                // Buffer BEFORE parsing: `process_chunk` finalizes on
                // `message_stop`, and finalize is what writes the entry — so a
                // chunk carrying `message_stop` has to already be in the buffer
                // or the replayed stream would be missing its terminator.
                this.buffer_for_cache(&bytes);
                this.process_chunk(&bytes);
                if this.pending_write.is_some() {
                    this.pending_terminal = Some(bytes);
                    this.poll_usage_write(cx).unwrap_or(Poll::Pending)
                } else {
                    Poll::Ready(Some(Ok(bytes)))
                }
            }
            Poll::Ready(Some(Err(e))) => {
                // Transparent passthrough: forward the error unchanged rather
                // than synthesizing an SSE event (unlike the OpenAI path).
                this.span
                    .set_error("stream_transport_error", Some(&e.to_string()));
                this.finalize();
                if let Some(future) = this.pending_write.take() {
                    usage::detach_future(future);
                }
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                // End of stream with no transport error: the response is whole.
                this.clean_end = true;
                this.finalize();
                if this.pending_write.is_some() {
                    this.end_after_write = true;
                    this.poll_usage_write(cx).unwrap_or(Poll::Pending)
                } else {
                    this.terminated = true;
                    Poll::Ready(None)
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for AnthropicSseSpanTap {
    fn drop(&mut self) {
        // A client disconnect drops the body mid-stream: record what was
        // accumulated and stamp a final end time on the way out.
        let span = self.span.clone();
        let _enter = span.enter();
        self.finalize();
        if let Some(future) = self.pending_write.take() {
            usage::detach_future(future);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_sse_reconstructs_the_live_outcome_response_shape() {
        let payload = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"role\":\"assistant\",\"usage\":{\"input_tokens\":3}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello!\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let response = anthropic_response_from_cached_sse(payload.as_bytes())
            .expect("complete cached SSE should assemble a response");
        assert_eq!(response["content"][0]["text"], "Hello!");
        assert_eq!(response["stop_reason"], "end_turn");
        assert_eq!(response["usage"]["input_tokens"], 3);
        assert_eq!(response["usage"]["output_tokens"], 2);
    }

    #[test]
    fn anthropic_requirements_detect_request_features_without_rejecting_unknown() {
        let request = serde_json::json!({
            "model": "custom/model",
            "stream": true,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}},
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "ok"}
                ]
            }]
        });
        let requirements = anthropic_requirements(AnthropicRequestEndpoint::Messages, &request);
        let mut profile = crate::capabilities::unknown_generation_capabilities("model");
        assert!(profile.satisfies(&requirements));
        profile
            .interfaces
            .get_mut(&crate::capabilities::Interface::AnthropicMessages)
            .unwrap()
            .operations
            .get_mut(&crate::capabilities::InterfaceOperation::Create)
            .unwrap()
            .features
            .insert(
                EndpointFeature::ImageInput,
                crate::capabilities::Support::Unsupported,
            );
        assert!(!profile.satisfies(&requirements));
    }
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
    use opentelemetry_sdk::trace::{SimpleSpanProcessor, TracerProvider};
    use std::collections::HashMap;
    use tracing_subscriber::layer::SubscriberExt;

    fn resolution() -> RouteResolution {
        RouteResolution {
            provider_kind: "anthropic".to_string(),
            connection_id: Some("id-1".to_string()),
            connection_name: "anthropic".to_string(),
            upstream_model_id: "claude-3-5-sonnet".to_string(),
            connection_prefix: None,
            model_route_id: None,
        }
    }

    fn request_span_attributes(request: serde_json::Value) -> HashMap<String, String> {
        let mut route = resolution();
        if let Some(model) = request.get("model").and_then(|model| model.as_str()) {
            route.upstream_model_id = model.to_string();
        }
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer().with_tracer(provider.tracer("anthropic-request-test")),
        );

        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let span = tracing::info_span!("anthropic_request_attributes");
            let span = LlmSpanWriter::new(span, None, LlmSpanKind::Llm);
            let body = Bytes::from(serde_json::to_vec(&request).unwrap());
            set_anthropic_request_span_attributes(&span, &body, &route);
            drop(span);
        }
        for result in provider.force_flush() {
            result.unwrap();
        }
        exporter
            .get_finished_spans()
            .unwrap()
            .into_iter()
            .find(|span| span.name.as_ref() == "anthropic_request_attributes")
            .unwrap()
            .attributes
            .into_iter()
            .map(|attribute| (attribute.key.to_string(), attribute.value.to_string()))
            .collect()
    }

    fn perm(
        provider: Option<&str>,
        connection_id: Option<&str>,
        models: Option<Vec<&str>>,
    ) -> ApiKeyPermission {
        ApiKeyPermission {
            provider: provider.map(str::to_string),
            connection_id: connection_id.map(str::to_string),
            models: models.map(|m| m.into_iter().map(str::to_string).collect()),
            ..Default::default()
        }
    }

    #[test]
    fn request_model_reads_the_body_model_field() {
        let body = Bytes::from(r#"{"model":"claude-3-5-sonnet","messages":[]}"#);
        assert_eq!(request_model(&body).as_deref(), Some("claude-3-5-sonnet"));
        // No model / unparseable body -> None.
        assert_eq!(request_model(&Bytes::from(r#"{"messages":[]}"#)), None);
        assert_eq!(request_model(&Bytes::from("not json")), None);
    }

    #[test]
    fn rewrite_request_model_reports_non_object_bodies() {
        for body in ["[]", "null", "true", "42", r#""text""#] {
            assert_eq!(
                rewrite_request_model(&Bytes::from(body), "claude").unwrap_err(),
                "Anthropic request body must be a JSON object"
            );
        }
    }

    #[test]
    fn rewrite_request_model_preserves_other_fields() {
        let body = Bytes::from(
            r#"{"model":"anthropic/claude-sonnet-5","messages":[],"future_option":{"enabled":true}}"#,
        );
        let rewritten = rewrite_request_model(&body, "claude-sonnet-5").unwrap();
        let value: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(value["model"], "claude-sonnet-5");
        assert_eq!(value["future_option"]["enabled"], true);
    }

    #[test]
    fn unrestricted_key_passes_without_a_model() {
        // An unrestricted key never needs the model and is always allowed.
        assert!(check_permission(&perm(None, None, None), &resolution(), None).is_ok());
    }

    #[test]
    fn permission_enforces_provider_and_connection_axes() {
        // A key scoped to another provider is denied even though it authenticated.
        assert!(matches!(
            check_permission(
                &perm(Some("openai"), None, None),
                &resolution(),
                Some("claude")
            ),
            Err(ApiKeyError::ProviderDenied { .. })
        ));
        // Scoped to "anthropic" -> allowed.
        assert!(check_permission(
            &perm(Some("anthropic"), None, None),
            &resolution(),
            Some("claude")
        )
        .is_ok());
        // Scoped to a different connection id -> denied; matching id -> allowed.
        assert!(matches!(
            check_permission(
                &perm(None, Some("other-id"), None),
                &resolution(),
                Some("claude")
            ),
            Err(ApiKeyError::ConnectionDenied)
        ));
        assert!(check_permission(
            &perm(None, Some("id-1"), None),
            &resolution(),
            Some("claude")
        )
        .is_ok());
    }

    #[test]
    fn model_scoped_key_fails_closed_when_model_absent() {
        let p = perm(None, None, Some(vec!["anthropic/claude-3-5-sonnet"]));
        // The public catalog id is authorized even though the resolved model is
        // provider-native.
        assert!(check_permission(&p, &resolution(), Some("anthropic/claude-3-5-sonnet")).is_ok());
        // Different model -> denied.
        assert!(matches!(
            check_permission(&p, &resolution(), Some("claude-3-opus")),
            Err(ApiKeyError::ModelDenied { .. })
        ));
        // Missing model on a model-scoped key -> denied (fail closed).
        assert!(matches!(
            check_permission(&p, &resolution(), None),
            Err(ApiKeyError::ModelDenied { .. })
        ));
    }

    #[test]
    fn anthropic_usage_tokens_reads_all_four_fields() {
        let usage = serde_json::json!({
            "input_tokens": 11,
            "output_tokens": 6,
            "cache_read_input_tokens": 4,
            "cache_creation_input_tokens": 2,
        });
        assert_eq!(
            anthropic_usage_tokens(&usage),
            AnthropicUsageTokens {
                input_tokens: 11,
                output_tokens: 6,
                cache_read_input_tokens: 4,
                cache_creation_input_tokens: 2,
            }
        );
        // True prompt size sums all three input-side fields — `input_tokens`
        // alone excludes both cache fields.
        assert_eq!(anthropic_usage_tokens(&usage).prompt_tokens(), 17);
    }

    #[test]
    fn anthropic_usage_tokens_defaults_absent_fields_to_zero() {
        // `message_delta.usage` in practice carries only `output_tokens`.
        let usage = serde_json::json!({"output_tokens": 7});
        assert_eq!(
            anthropic_usage_tokens(&usage),
            AnthropicUsageTokens {
                output_tokens: 7,
                ..Default::default()
            }
        );

        // `message_start.message.usage` carries `input_tokens` and, only when
        // prompt caching was used, the two cache fields.
        let usage = serde_json::json!({"input_tokens": 12});
        assert_eq!(
            anthropic_usage_tokens(&usage),
            AnthropicUsageTokens {
                input_tokens: 12,
                ..Default::default()
            }
        );

        assert_eq!(
            anthropic_usage_tokens(&serde_json::json!({})),
            AnthropicUsageTokens::default()
        );
    }

    #[test]
    fn anthropic_usage_tokens_ignores_unrelated_fields() {
        // Non-numeric / unrelated fields don't panic and don't leak into the
        // extracted counts.
        let usage = serde_json::json!({
            "input_tokens": 5,
            "service_tier": "standard",
        });
        assert_eq!(
            anthropic_usage_tokens(&usage),
            AnthropicUsageTokens {
                input_tokens: 5,
                ..Default::default()
            }
        );
    }

    #[test]
    fn invocation_parameters_allowlist_excludes_client_metadata_and_extensions() {
        let request = serde_json::json!({
            "model": "claude-test",
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 64,
            "temperature": 0.25,
            "metadata": {"user_id": "private-user"},
            "extension": "x".repeat(MAX_INVOCATION_PARAMETERS_BYTES),
        });
        let (parameters, omitted) =
            bounded_anthropic_invocation_parameters(request.as_object().unwrap());
        let parameters: serde_json::Value =
            serde_json::from_str(&parameters.expect("standard parameters should remain")).unwrap();

        assert_eq!(parameters["max_tokens"], 64);
        assert_eq!(parameters["temperature"], 0.25);
        assert!(parameters.get("metadata").is_none());
        assert!(parameters.get("extension").is_none());
        assert_eq!(omitted, 0);
    }

    #[test]
    fn oversized_model_name_is_omitted_with_truncation_metadata() {
        let model = "m".repeat(super::super::trace_attributes::MAX_INLINE_CONTENT_BYTES + 1);
        let attributes = request_span_attributes(serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 64,
        }));

        assert!(!attributes.contains_key("llm.model_name"));
        let truncation: serde_json::Value = serde_json::from_str(
            attributes
                .get("chalk.trace.input.truncation")
                .expect("oversized model omission should be reported"),
        )
        .unwrap();
        assert_eq!(
            truncation["model_name"]["bytes"],
            super::super::trace_attributes::MAX_INLINE_CONTENT_BYTES + 1
        );
    }

    #[test]
    fn anthropic_input_budget_keeps_newest_messages() {
        crate::handlers::init_content_logging(true);
        let messages = (0..50)
            .map(|index| {
                serde_json::json!({
                    "role": "user",
                    "content": format!("message-{index}"),
                })
            })
            .collect::<Vec<_>>();
        let attributes = request_span_attributes(serde_json::json!({
            "model": "claude-test",
            "system": "keep this system prompt",
            "messages": messages,
            "max_tokens": 64,
        }));

        assert!(!attributes.contains_key("llm.input_messages.0.message.content"));
        assert_eq!(
            attributes
                .get("llm.input_messages.50.message.content")
                .map(String::as_str),
            Some("message-49")
        );
    }
}
