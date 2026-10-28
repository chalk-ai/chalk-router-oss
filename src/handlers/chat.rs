use crate::backend::provider::{BackendChatResponse, BackendSseResponse};
use crate::native_chat::NativeChatRequest;
use crate::tenancy::RequestState as State;
use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::backend::{BackendError, BackendProvider, RouteResolution, SseByteStream};
use crate::middleware::ResolvedApiKey;
use crate::prompt_cache::{CacheContext, CachedCompletion, PayloadKind};
use crate::quota::{QuotaPermit, TokenReservation};
use crate::storage;
use crate::types::{
    ChatChoice, ChatCompletionChunk, ChatCompletionChunkChoice, ChatCompletionDelta,
    ChatCompletionRequest, ChatCompletionResponse, ChatMessage, Error, ErrorResponse, FunctionCall,
    ToolCall, Usage,
};
use crate::DebugCompletionEntry;
use axum::{
    body::{Body, Bytes},
    extract::Json,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
};
use futures::{stream, Stream};

use super::routing::{set_route_span_attributes, stamp_routing_headers, try_with_fallbacks};
use super::sse::{done_frame, encode_frame, SseDecoder, SseFrame};
use super::trace_attributes::{
    ContentDirection, LlmSpanKind, LlmSpanWriter, MAX_INVOCATION_PARAMETERS_BYTES,
};
use super::usage::{TokenUsage, UsageAttribution, UsageWriteFuture};
use crate::usage_estimator::{NormalizedUsageInput, UsageEstimateState};

/// Open one candidate's stream under the shared stream-open deadline.
///
/// An elapsed deadline is reported as a synthesized `504`, so a configured
/// fallback can advance and the caller still receives an honest status if every
/// candidate times out. Dropping the timeout future cancels the in-flight open,
/// so a stalled attempt does not leak.
async fn open_stream_with_deadline(
    backend: Arc<dyn BackendProvider>,
    req: NativeChatRequest,
) -> Result<BackendSseResponse, BackendError> {
    super::routing::open_stream_with_deadline(backend.create_native_chat_completion_stream(req))
        .await
}

/// Extract message content as a plain string, handling both string and array-of-parts formats.
fn message_content_to_string(content: &Option<serde_json::Value>) -> Option<String> {
    match content {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Array(arr)) => {
            let text: String = arr
                .iter()
                .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
        _ => None,
    }
}

/// OpenInference: stamp the response choices onto the span as output
/// attributes (`output.value`, `llm.output_messages.*`). Shared by the
/// non-streaming path (from the response body) and the streaming path (from
/// deltas assembled by [`SseSpanTap`]). Message content and tool-call
/// arguments are gated on `content_logging_enabled`; roles, tool-call ids, and
/// function names are always stamped.
fn set_output_span_attributes(span: &LlmSpanWriter, choices: &[ChatChoice]) {
    let log_content = span.content_enabled();
    // Fixed result metadata is stamped before any expandable output fields so
    // the SDK's first-N retention cannot evict it on a wide response.
    if let Some(finish_reason) = choices
        .iter()
        .find_map(|choice| choice.finish_reason.as_deref())
    {
        span.set_finish_reason(finish_reason);
    }

    span.record_json(ContentDirection::Output, &choices);

    let mut recorded_roles = 0usize;
    for (i, choice) in choices.iter().enumerate() {
        if !span.try_metadata(
            ContentDirection::Output,
            format!("llm.output_messages.{i}.message.role"),
            choice.message.role.clone(),
        ) {
            continue;
        }
        recorded_roles += 1;
    }

    // Tool-call metadata comes before prose so ids, names, and arguments are
    // retained before optional message content. The complete raw output is
    // still available inline or by content URI when flattened fields are cut.
    let total_tool_calls: usize = choices
        .iter()
        .map(|choice| choice.message.tool_calls.as_ref().map_or(0, Vec::len))
        .sum();
    let mut recorded_tool_calls = Vec::new();
    for (i, choice) in choices.iter().enumerate() {
        if let Some(ref tool_calls) = choice.message.tool_calls {
            for (tool_index, tc) in tool_calls.iter().enumerate() {
                if !span.try_metadata_group(
                    ContentDirection::Output,
                    [
                        (
                            format!("llm.output_messages.{i}.message.tool_calls.{tool_index}.tool_call.id"),
                            tc.id.clone(),
                        ),
                        (
                            format!("llm.output_messages.{i}.message.tool_calls.{tool_index}.tool_call.function.name"),
                            tc.function.name.clone(),
                        ),
                    ],
                ) {
                    continue;
                }
                recorded_tool_calls.push((i, tool_index, tc));
            }
        }
    }

    let mut omitted_tool_arguments = total_tool_calls.saturating_sub(recorded_tool_calls.len());
    if log_content {
        for (message_index, tool_index, tool_call) in &recorded_tool_calls {
            if !span.try_content(
                ContentDirection::Output,
                    format!("llm.output_messages.{message_index}.message.tool_calls.{tool_index}.tool_call.function.arguments"),
                    tool_call.function.arguments.clone(),
                ) {
                omitted_tool_arguments += 1;
            }
        }
    } else {
        omitted_tool_arguments = 0;
    }

    let mut omitted_message_content = 0usize;
    for (i, choice) in choices.iter().enumerate() {
        if log_content {
            if let Some(text) = message_content_to_string(&choice.message.content) {
                if !span.try_content(
                    ContentDirection::Output,
                    format!("llm.output_messages.{i}.message.content"),
                    text,
                ) {
                    omitted_message_content += 1;
                }
            }
        }
    }

    if recorded_roles < choices.len() || omitted_message_content > 0 {
        span.truncation(
            ContentDirection::Output,
            "messages",
            serde_json::json!({
                "total": choices.len(),
                "roles_recorded": recorded_roles,
                "content_fields_omitted": omitted_message_content,
            }),
        );
    }
    if recorded_tool_calls.len() < total_tool_calls || omitted_tool_arguments > 0 {
        span.truncation(
            ContentDirection::Output,
            "tool_calls",
            serde_json::json!({
                "total": total_tool_calls,
                "recorded": recorded_tool_calls.len(),
                "arguments_omitted": omitted_tool_arguments,
            }),
        );
    }
}

/// Non-streaming dispatch for [`try_with_fallbacks`].
///
/// Deliberately *not* deadline-bounded: this future resolves only after the full
/// generation, so a fixed bound here would abort healthy long completions. The
/// streaming counterpart bounds only the connection-open, which is why it can
/// afford a deadline — see [`STREAM_OPEN_TIMEOUT`].
async fn dispatch_completion(
    backend: Arc<dyn BackendProvider>,
    req: NativeChatRequest,
) -> Result<BackendChatResponse, BackendError> {
    backend.create_native_chat_completion(req).await
}

/// Capture the complete typed request as JSON, plus bounded OpenInference previews.
///
/// `llm.system` / `llm.model_name` are intentionally NOT set here: they describe
/// the resolved upstream connection, which is only known after routing, so they
/// are set from the `RouteResolution` once the request has been resolved.
fn set_input_span_attributes(span: &LlmSpanWriter, req: &NativeChatRequest, log_content: bool) {
    // The native body is authoritative: start from every raw control so
    // provider-specific ones are traced, then let the typed view overwrite the
    // ones with retained schema order.
    let value = req.value();
    let mut params: std::collections::BTreeMap<&str, crate::schema_json::SchemaJson> = value
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(name, _)| !matches!(name.as_str(), "model" | "messages"))
        .map(|(name, value)| (name.as_str(), value.clone().into()))
        .collect();
    if let Some(t) = req.temperature {
        params.insert("temperature", serde_json::json!(t).into());
    }
    if let Some(tp) = req.top_p {
        params.insert("top_p", serde_json::json!(tp).into());
    }
    if let Some(mt) = req.max_tokens {
        params.insert("max_tokens", serde_json::json!(mt).into());
    }
    if let Some(mct) = req.max_completion_tokens {
        params.insert("max_completion_tokens", serde_json::json!(mct).into());
    }
    if let Some(n) = req.n {
        params.insert("n", serde_json::json!(n).into());
    }
    if let Some(ref stop) = req.stop {
        params.insert("stop", serde_json::json!(stop).into());
    }
    if let Some(pp) = req.presence_penalty {
        params.insert("presence_penalty", serde_json::json!(pp).into());
    }
    if let Some(fp) = req.frequency_penalty {
        params.insert("frequency_penalty", serde_json::json!(fp).into());
    }
    if let Some(ref tc) = req.tool_choice {
        params.insert("tool_choice", tc.clone().into());
    }
    if let Some(ref rf) = req.response_format {
        params.insert("response_format", rf.clone());
    }
    let invocation_parameters_omitted = if !params.is_empty() {
        match serde_json::to_string(&params) {
            Ok(params_json) if params_json.len() <= MAX_INVOCATION_PARAMETERS_BYTES => {
                !span.set_invocation_parameters_json(params_json)
            }
            Ok(_) => true,
            Err(_) => false,
        }
    } else {
        false
    };

    if log_content {
        span.record_json(ContentDirection::Input, req);
    }
    if invocation_parameters_omitted {
        span.truncation(
            ContentDirection::Input,
            "invocation_parameters",
            serde_json::json!({"max_bytes": MAX_INVOCATION_PARAMETERS_BYTES}),
        );
    }
    let tool_count = req.tools.as_ref().map_or(0, Vec::len);
    let reserved_tool_attributes = if log_content && tool_count > 0 {
        let (input_attribute_limit, _) = super::trace_attributes::llm_attribute_budgets();
        tool_count.min(input_attribute_limit / 3)
    } else {
        0
    };
    let reserved_tool_content_bytes = if reserved_tool_attributes > 0 {
        span.content_bytes_remaining(ContentDirection::Input) / 2
    } else {
        0
    };

    // Prefer the newest messages while retaining their original source
    // indices. Hold back a share of both attribute and byte capacity for
    // advertised tools.
    let mut planned_messages = Vec::new();
    let mut omitted_message_metadata = 0usize;
    let mut omitted_message_content = 0usize;

    let message_order = (0..req.messages.len()).rev();
    for i in message_order {
        let msg = &req.messages[i];
        if !span.try_metadata_preserving(
            ContentDirection::Input,
            format!("llm.input_messages.{i}.message.role"),
            msg.role.clone(),
            reserved_tool_attributes,
            reserved_tool_content_bytes,
        ) {
            omitted_message_metadata += 1;
            continue;
        }

        if let Some(ref name) = msg.name {
            if !span.try_metadata_preserving(
                ContentDirection::Input,
                format!("llm.input_messages.{i}.message.name"),
                name.clone(),
                reserved_tool_attributes,
                reserved_tool_content_bytes,
            ) {
                omitted_message_metadata += 1;
            }
        }
        if let Some(ref tool_call_id) = msg.tool_call_id {
            if !span.try_metadata_preserving(
                ContentDirection::Input,
                format!("llm.input_messages.{i}.message.tool_call_id"),
                tool_call_id.clone(),
                reserved_tool_attributes,
                reserved_tool_content_bytes,
            ) {
                omitted_message_metadata += 1;
            }
        }
        if let Some(ref tool_calls) = msg.tool_calls {
            for (j, tool_call) in tool_calls.iter().enumerate() {
                if span.try_metadata_group_preserving(
                    ContentDirection::Input,
                    [
                        (
                            format!("llm.input_messages.{i}.message.tool_calls.{j}.tool_call.id"),
                            tool_call.id.clone(),
                        ),
                        (
                            format!("llm.input_messages.{i}.message.tool_calls.{j}.tool_call.function.name"),
                            tool_call.function.name.clone(),
                        ),
                    ],
                    reserved_tool_attributes,
                    reserved_tool_content_bytes,
                ) {
                    if log_content
                        && !span.try_content_preserving(
                            ContentDirection::Input,
                            format!("llm.input_messages.{i}.message.tool_calls.{j}.tool_call.function.arguments"),
                            tool_call.function.arguments.clone(),
                            reserved_tool_attributes,
                            reserved_tool_content_bytes,
                        )
                    {
                        omitted_message_content += 1;
                    }
                } else {
                    omitted_message_metadata += 2;
                    if log_content {
                        omitted_message_content += 1;
                    }
                }
            }
        }
        if log_content {
            if let Some(text) = message_content_to_string(&msg.content) {
                if !span.try_content_preserving(
                    ContentDirection::Input,
                    format!("llm.input_messages.{i}.message.content"),
                    text,
                    reserved_tool_attributes,
                    reserved_tool_content_bytes,
                ) {
                    omitted_message_content += 1;
                }
            }
        }
        planned_messages.push(i);
    }

    let recorded_messages = planned_messages.len();
    if recorded_messages < req.messages.len()
        || omitted_message_metadata > 0
        || omitted_message_content > 0
    {
        span.truncation(
            ContentDirection::Input,
            "messages",
            serde_json::json!({
                "total": req.messages.len(),
                "recorded": recorded_messages,
                "omitted": req.messages.len().saturating_sub(recorded_messages),
                "omitted_before": planned_messages
                    .iter()
                    .copied()
                    .min()
                    .unwrap_or(req.messages.len()),
                "source_start_index": planned_messages
                    .iter()
                    .copied()
                    .min(),
                "source_end_index": planned_messages
                    .iter()
                    .copied()
                    .max(),
                "metadata_fields_omitted": omitted_message_metadata,
                "content_fields_omitted": omitted_message_content,
            }),
        );
    }

    let mut recorded_tools = 0usize;
    let mut omitted_tool_schemas = 0usize;
    if log_content {
        if let Some(ref tools) = req.tools {
            for (source_index, tool) in tools.iter().enumerate() {
                let Ok(json) = serde_json::to_string(tool) else {
                    omitted_tool_schemas += 1;
                    continue;
                };
                if !span.try_content(
                    ContentDirection::Input,
                    format!("llm.tools.{source_index}.tool.json_schema"),
                    json,
                ) {
                    omitted_tool_schemas += 1;
                    continue;
                }
                recorded_tools += 1;
            }
        }
        if recorded_tools < tool_count {
            span.truncation(
                ContentDirection::Input,
                "tools",
                serde_json::json!({
                    "total": tool_count,
                    "recorded": recorded_tools,
                    "schemas_omitted": omitted_tool_schemas,
                }),
            );
        }
    }
}

fn indexed_image_urls(req: &ChatCompletionRequest) -> impl Iterator<Item = (usize, &str)> {
    req.messages
        .iter()
        .flat_map(|message| match &message.content {
            Some(serde_json::Value::Array(parts)) => parts.as_slice(),
            _ => &[],
        })
        .filter_map(|part| {
            (part.get("type").and_then(|value| value.as_str()) == Some("image_url"))
                .then(|| {
                    part.get("image_url")
                        .and_then(|image| image.get("url"))
                        .and_then(|url| url.as_str())
                })
                .flatten()
        })
        .enumerate()
}

/// Upload images from message content to object storage and set image.url spans.
async fn upload_images(span: &LlmSpanWriter, req: &ChatCompletionRequest, state: &crate::AppState) {
    let mut omitted_images = 0usize;
    for (image_index, url) in indexed_image_urls(req) {
        if let Some(ref store) = state.object_store {
            if !span.can_record_attributes(ContentDirection::Input, 1) {
                omitted_images += 1;
                continue;
            }
            match storage::fetch_image_bytes(url).await {
                Ok((bytes, ext)) => match store.put_hashed(&bytes, &ext).await {
                    Ok(key) => {
                        let uri = store.uri_for_key(&key);
                        if !span.try_content(
                            ContentDirection::Input,
                            format!("image.{image_index}.url"),
                            uri,
                        ) {
                            omitted_images += 1;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to upload chat image to object store");
                    }
                },
                Err(e) => {
                    tracing::warn!(error = %e, url = %url, "Failed to fetch chat image");
                }
            }
        }
    }
    if omitted_images > 0 {
        span.truncation(
            ContentDirection::Input,
            "images",
            serde_json::json!({"attributes_omitted": omitted_images}),
        );
    }
}

// ---------------------------------------------------------------------------
// Exact-prefix response cache
// ---------------------------------------------------------------------------
//
// Simple routes consult the cache after permission checks. Routes with a grid
// and requests with overrides bypass response caching so a stored response
// cannot bypass current selection, permissions, or the requested recovery plan.
// Classifier verdict caching is separate and includes the route judge config.
//
// Both shapes are served from the *same* entry: the stored value is always the
// canonical non-streamed `ChatCompletionResponse`, and a streaming hit is
// re-emitted as SSE by `create_sse_response`. See `crate::prompt_cache`.

/// Build the client response for a cache hit, or `None` if the stored payload
/// is not a chat completion after all (a foreign or corrupt entry) — in which
/// case the caller falls through to a normal, uncached dispatch rather than
/// returning something malformed.
///
/// The non-streaming reply serves the stored bytes **verbatim** rather than
/// re-serializing the parsed value, so a cached answer is byte-identical to the
/// one the upstream originally produced (field order, number formatting, and
/// any provider extension fields included).
fn cache_hit_response(
    span: &LlmSpanWriter,
    sampling_gate: &std::sync::Arc<crate::telemetry::sampling::SamplingGate>,
    backend: &'static str,
    ctx: &CacheContext,
    value: &CachedCompletion,
    request: &NativeChatRequest,
    requested_model: &str,
) -> Option<Response> {
    let streaming = request.stream.unwrap_or(false);
    if value.payload_kind != PayloadKind::Json {
        return None;
    }
    let mut parsed: ChatCompletionResponse = serde_json::from_slice(&value.payload).ok()?;
    if streaming {
        let raw: serde_json::Value = serde_json::from_slice(&value.payload).ok()?;
        if !chat_cache_shape_is_supported(&raw, false) {
            return None;
        }
        if !request.include_usage() {
            parsed.usage = None;
        }
    }

    // Replaying the stored token counts makes the span useful, but this request
    // made no provider call. Pin its upstream cost to zero before usage is
    // stamped so list-price estimation cannot charge for the cached payload.
    span.suppress_provider_cost_for_cache_hit();

    // A hit still gets the full LLM span treatment — output messages, token
    // counts — because a trace of a cached turn should show what the caller
    // actually received, not an empty span. What it does NOT get is usage
    // *recording*: no upstream call was made, so no tokens were bought, and
    // billing a cache hit against the key's ledger would double-charge the
    // completion that populated it.
    set_output_span_attributes(span, &parsed.choices);
    span.apply_chat_outcome_sampling(sampling_gate, &parsed.choices, Some(&parsed));
    if let Some(ref usage) = parsed.usage {
        span.set_token_usage(
            usage.prompt_tokens.max(0) as u64,
            usage.completion_tokens.map(|c| c.max(0) as u64),
            usage.total_tokens.max(0) as u64,
            0,
            0,
            0,
        );
    }

    let mut res = if streaming {
        create_sse_response(parsed)
    } else {
        let mut res = Response::new(Body::from(value.payload.clone()));
        res.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        res
    };
    crate::handlers::stamp_trace_headers(res.headers_mut());
    crate::handlers::stamp_cache_outcome(
        span,
        Some(res.headers_mut()),
        backend,
        ctx,
        Some(value.age()),
    );
    // Report the routing the *cached* call took: a judge selection or a
    // fallback may have moved it off the requested model, and a caller reading
    // `x-chalk-routed-model` should see the same thing it saw when the entry
    // was live.
    if value.served_model != requested_model {
        if let Ok(val) = HeaderValue::from_str(&value.served_model) {
            res.headers_mut().insert("x-chalk-routed-model", val);
        }
    }
    Some(res)
}

pub async fn create_chat_completion(
    State(state): State<crate::AppState>,
    api_key: ResolvedApiKey,
    headers: HeaderMap,
    Json(req): Json<NativeChatRequest>,
) -> Response {
    let mut req = req.with_headers(&headers);
    if let Err(error) = crate::route_grid::validate_fallbacks(&req.fallbacks) {
        return super::backend_error_response(&BackendError::api_status(400, error));
    }
    match state
        .backend
        .normalize_request_fallbacks(&req.fallbacks)
        .await
    {
        Ok(fallbacks) => req.set_fallbacks(fallbacks),
        Err(error) => return super::backend_error_response(&error),
    }
    let is_streaming = req.stream.unwrap_or(false);

    let span = tracing::info_span!("chat_completion", otel.name = "chat.completion",);
    let span = LlmSpanWriter::new(span, state.object_store.clone(), LlmSpanKind::Llm);
    super::stamp_authenticated_principal(&span, &api_key);

    span.clone()
        .instrument(async move {
            let current_span = span.clone();
            let log_content = current_span.content_enabled();
            set_input_span_attributes(&current_span, &req, log_content);
            if log_content {
                upload_images(&current_span, &req, &state).await;
            }

            let original_model = req.model.clone();
            let debug_message_count = req.messages.len();
            let start = std::time::Instant::now();

            if is_streaming {
                handle_streaming(
                    current_span,
                    state,
                    api_key,
                    req,
                    original_model,
                    debug_message_count,
                    start,
                )
                .await
            } else {
                handle_non_streaming(
                    current_span,
                    state,
                    api_key,
                    req,
                    original_model,
                    debug_message_count,
                    start,
                )
                .await
            }
        })
        .await
}

/// Cache conversion is deliberately conservative: unfamiliar reply semantics
/// stay on the live native path instead of being replayed as an empty answer.
fn chat_cache_shape_is_supported(value: &serde_json::Value, streaming: bool) -> bool {
    // Unknown keys only block caching when they carry something a replay would
    // lose. Providers routinely send `logprobs:null`, `refusal:null` or
    // `annotations:[]`; those replay identically without the key.
    fn inert(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Null => true,
            serde_json::Value::Array(items) => items.is_empty(),
            serde_json::Value::Object(fields) => fields.is_empty(),
            _ => false,
        }
    }
    fn keys(value: &serde_json::Value, allowed: &[&str]) -> bool {
        value.as_object().is_some_and(|object| {
            object
                .iter()
                .all(|(key, value)| allowed.contains(&key.as_str()) || inert(value))
        })
    }
    // `system_fingerprint` and `service_tier` are informational. Non-stream
    // hits replay the stored bytes verbatim; rebuilt replays omit them.
    if !keys(
        value,
        &[
            "id",
            "object",
            "created",
            "model",
            "choices",
            "usage",
            "system_fingerprint",
            "service_tier",
        ],
    ) {
        return false;
    }
    if let Some(usage) = value.get("usage").filter(|value| !value.is_null()) {
        if !keys(
            usage,
            &[
                "prompt_tokens",
                "completion_tokens",
                "total_tokens",
                "prompt_tokens_details",
                "completion_tokens_details",
            ],
        ) {
            return false;
        }
        if usage
            .get("prompt_tokens_details")
            .filter(|value| !value.is_null())
            .is_some_and(|value| !keys(value, &["cached_tokens", "cache_write_tokens"]))
        {
            return false;
        }
        if usage
            .get("completion_tokens_details")
            .filter(|value| !value.is_null())
            .is_some_and(|value| !keys(value, &["reasoning_tokens"]))
        {
            return false;
        }
    }
    value
        .get("choices")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|choices| {
            choices.iter().all(|choice| {
                let field = if streaming { "delta" } else { "message" };
                keys(choice, &["index", field, "finish_reason"])
                    && choice.get(field).is_some_and(|message| {
                        keys(
                            message,
                            if streaming {
                                &["role", "content", "tool_calls"]
                            } else {
                                &["role", "content", "tool_calls", "name", "tool_call_id"]
                            },
                        ) && message
                            .get("content")
                            .is_none_or(|value| value.is_null() || value.is_string())
                            && message
                                .get("tool_calls")
                                .filter(|value| !value.is_null())
                                .is_none_or(|value| {
                                    value.as_array().is_some_and(|calls| {
                                        calls.iter().all(|call| {
                                            keys(
                                                call,
                                                if streaming {
                                                    &["index", "id", "type", "function"]
                                                } else {
                                                    &["id", "type", "function"]
                                                },
                                            ) && call.get("function").is_none_or(|function| {
                                                function.is_null()
                                                    || keys(function, &["name", "arguments"])
                                            })
                                        })
                                    })
                                })
                    })
            })
        })
}

fn is_usage_only_frame(frame: &SseFrame) -> bool {
    frame.event.is_none()
        && serde_json::from_str::<serde_json::Value>(&frame.data)
            .ok()
            .is_some_and(|value| {
                value.get("usage").is_some_and(|usage| !usage.is_null())
                    && value
                        .get("choices")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(Vec::is_empty)
            })
}

/// Hide only internally requested usage-only events. The shared decoder finds
/// complete frames; all other frame bytes, including comments and IDs, survive.
fn filter_usage_chunks<S>(
    stream: S,
    include_usage: bool,
) -> impl Stream<Item = Result<bytes::Bytes, std::convert::Infallible>>
where
    S: Stream<Item = Result<bytes::Bytes, std::convert::Infallible>> + Send + 'static,
{
    use futures::StreamExt;
    let stream: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, std::convert::Infallible>> + Send>> =
        Box::pin(stream);
    futures::stream::unfold(
        (
            stream,
            SseDecoder::default(),
            Vec::new(),
            std::collections::VecDeque::new(),
            false,
        ),
        move |(mut stream, mut decoder, mut wire, mut ready, mut ended)| async move {
            loop {
                if let Some(bytes) = ready.pop_front() {
                    return Some((Ok(bytes), (stream, decoder, wire, ready, ended)));
                }
                if ended {
                    return None;
                }
                match stream.next().await {
                    Some(Ok(bytes)) if include_usage => {
                        return Some((Ok(bytes), (stream, decoder, wire, ready, ended)))
                    }
                    Some(Ok(bytes)) => {
                        for line in bytes.split_inclusive(|byte| *byte == b'\n') {
                            wire.extend_from_slice(line);
                            let frames = decoder.push(line);
                            if !frames.is_empty() || decoder.pending_bytes() == 0 {
                                let count = wire.len().saturating_sub(decoder.pending_bytes());
                                let original =
                                    bytes::Bytes::from(wire.drain(..count).collect::<Vec<_>>());
                                let usage_only =
                                    frames.len() == 1 && is_usage_only_frame(&frames[0]);
                                if !usage_only {
                                    ready.push_back(original);
                                }
                            }
                        }
                    }
                    None => {
                        ended = true;
                        let frames = decoder.finish();
                        if !wire.is_empty()
                            && !(frames.len() == 1 && is_usage_only_frame(&frames[0]))
                        {
                            ready.push_back(bytes::Bytes::from(std::mem::take(&mut wire)));
                        }
                    }
                    Some(Err(never)) => match never {},
                }
            }
        },
    )
}

/// Handle streaming chat completions by passing through SSE bytes from the upstream provider.
async fn handle_streaming(
    current_span: LlmSpanWriter,
    state: crate::AppState,
    api_key: ResolvedApiKey,
    req: NativeChatRequest,
    original_model: String,
    debug_message_count: usize,
    start: std::time::Instant,
) -> Response {
    let client_include_usage = req.include_usage();
    let normalized_usage_input = req.normalized_usage_input();
    // Opaque parts leave admission a soft cap over the visible input.
    let admission_tokens = normalized_usage_input
        .or_else(|| req.soft_cap_admission_input())
        .map(|input| input.conservative_tokens());
    // Resolve the PRIMARY route ONCE up front. This resolution is carried through
    // the permission gate, span tagging, and dispatch; on a win the span and
    // usage tap are re-stamped from whichever candidate actually served
    // (RFP-0002 §5.6). An unresolvable model fails before we commit a 200
    // (returned as a real non-2xx, like the stream-open failure below).
    let route = match state.backend.resolve(&original_model).await {
        Ok(r) => r,
        Err(e) => {
            let duration_ms = start.elapsed().as_millis() as u64;
            tracing::error!(error = %e, "Chat completion stream routing error");
            state
                .debug_log
                .push(DebugCompletionEntry {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    model: original_model,
                    message_count: debug_message_count,
                    streaming: true,
                    status: "error".to_string(),
                    duration_ms,
                    usage: None,
                    error: Some(e.to_string()),
                })
                .await;
            return crate::handlers::backend_error_response(&e);
        }
    };
    let resolution = route.resolution.clone();

    // Stamp the span from the resolution NOW — before the permission gate and
    // dispatch — so a denied/errored stream span still carries system/model/
    // connection.
    set_route_span_attributes(&current_span, &resolution);
    // OpenInference-adjacent: distinguish streamed from non-streamed spans even
    // when denied/errored before a stream ever opens.
    current_span.set_stream(true);

    // Enforce the key's provider/model allowlist on the PRIMARY before opening
    // the stream. A denial here is terminal (403) — we do not fall back to
    // alternates the key also isn't permitted to reach; each alternate is gated
    // separately inside `try_with_fallbacks`. Denials must land here — before the
    // 200/SSE bytes are committed — as a real 403.
    if api_key.0.permission.is_restricted() {
        if let Err(denied) = api_key
            .0
            .permission
            .allows_route(&resolution, &original_model)
        {
            tracing::warn!(
                key_prefix = %api_key.0.key_prefix,
                model = %original_model,
                reason = %denied,
                "Chat completion stream denied by API key permission"
            );
            state
                .debug_log
                .push(DebugCompletionEntry {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    model: original_model,
                    message_count: debug_message_count,
                    streaming: true,
                    status: "error".to_string(),
                    duration_ms: start.elapsed().as_millis() as u64,
                    usage: None,
                    error: Some(denied.to_string()),
                })
                .await;
            return crate::handlers::forbidden_response(denied.to_string());
        }
    }

    // Exact-prefix cache: consulted here, between the permission gate and the
    // judge (see the cache section above). A streaming request shares its
    // entries with the non-streaming form of the same prompt: what is stored is
    // the canonical completion, and a hit is re-emitted here as SSE.
    let cache_ctx = if route.routing.is_none() && req.fallbacks.is_empty() {
        state.native_chat_cache_context(&api_key.0, &req)
    } else {
        None
    };
    if let Some(ctx) = &cache_ctx {
        if let Some(value) = state.prompt_cache.get(&ctx.key).await {
            if let Some(res) = cache_hit_response(
                &current_span,
                &state.sampling_gate,
                state.prompt_cache.kind(),
                ctx,
                &value,
                &req,
                &original_model,
            ) {
                let cache_permit = match super::routing::acquire_request_quota(
                    &state,
                    &api_key,
                    TokenReservation::NotApplicable,
                )
                .await
                {
                    Ok(permit) => permit,
                    Err(error) => return crate::handlers::backend_error_response(&error),
                };
                if let Some(permit) = cache_permit {
                    permit.finish(None).await;
                }
                state
                    .debug_log
                    .push(DebugCompletionEntry {
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        model: value.served_model.clone(),
                        message_count: debug_message_count,
                        streaming: true,
                        status: "cached".to_string(),
                        duration_ms: start.elapsed().as_millis() as u64,
                        usage: None,
                        error: None,
                    })
                    .await;
                return res;
            }
        }
    }

    let selection = match crate::judge::routing::select(
        &state,
        &api_key,
        &original_model,
        &req.fallbacks,
        route,
        || crate::judge::input::JudgeInput::chat(&req),
        &current_span,
    )
    .await
    {
        Ok(selection) => selection,
        Err(error) => return super::backend_error_response(&error),
    };
    let served_model = selection.plan.primary_model.clone();

    let result = try_with_fallbacks(
        &current_span,
        &state,
        req,
        selection.plan,
        &api_key,
        admission_tokens,
        open_stream_with_deadline,
    )
    .await;

    if result.is_ok() {
        crate::judge::routing::spawn_shadow_if_enabled(
            state.clone(),
            api_key.clone(),
            selection.shadow,
            current_span.clone(),
        );
    }
    match result {
        Ok((upstream, fallback_model, resolution, rate_limit_permit)) => {
            let BackendSseResponse {
                stream: byte_stream,
                headers: upstream_headers,
                status: upstream_status,
                content_type: upstream_content_type,
            } = upstream;
            // Attribute to the WINNING candidate. The up-front stamp was the
            // route default's, which is not necessarily what
            // served, so re-stamp from the resolution `try_with_fallbacks`
            // returned — never re-resolved.
            set_route_span_attributes(&current_span, &resolution);
            let used_model = fallback_model
                .as_deref()
                .unwrap_or(&served_model)
                .to_string();
            // Record the final served model, including fallback after selection.
            if used_model != original_model {
                current_span.set_requested_served_models(&original_model, &used_model);
            }

            // Tap the passthrough bytes: keeps the span's recorded end time
            // advancing while the body streams, assembles the streamed deltas
            // into output-message attributes, and extracts usage from the
            // final chunk (see `SseSpanTap`). Fed the WINNER's provider/model so
            // token counts are attributed to the connection that served.
            let tapped_stream = SseSpanTap::new(
                byte_stream,
                current_span.clone(),
                state.clone(),
                &api_key,
                &resolution,
                start,
                normalized_usage_input,
                // The tap doubles as the cache writer: it is already assembling
                // the streamed deltas into a whole completion for the span, and
                // that assembled completion is exactly what gets stored — so a
                // streamed miss populates the same entry a non-streamed one
                // would (see `SseSpanTap::finalize`).
                cache_ctx.clone().filter(|_| {
                    upstream_status == 200 && upstream_content_type == "text/event-stream"
                }),
                used_model.clone(),
                rate_limit_permit,
            );

            let body = Body::from_stream(filter_usage_chunks(tapped_stream, client_include_usage));

            state
                .debug_log
                .push(DebugCompletionEntry {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    // The model that actually serves the stream — a judge selection
                    // or a fallback may differ from the request.
                    model: used_model.clone(),
                    message_count: debug_message_count,
                    streaming: true,
                    status: "ok".to_string(),
                    duration_ms: start.elapsed().as_millis() as u64,
                    usage: None,
                    error: None,
                })
                .await;

            let mut res = Response::builder()
                .status(upstream_status)
                .header(header::CONTENT_TYPE, upstream_content_type)
                .header(header::CACHE_CONTROL, "no-cache")
                .header(header::CONNECTION, "keep-alive")
                .body(body)
                .unwrap();
            crate::handlers::apply_forwarded_headers(res.headers_mut(), &upstream_headers);
            crate::handlers::stamp_trace_headers(res.headers_mut());
            if let Some(ctx) = &cache_ctx {
                crate::handlers::stamp_cache_outcome(
                    &current_span,
                    Some(res.headers_mut()),
                    state.prompt_cache.kind(),
                    ctx,
                    None,
                );
            }
            // Surface routing to the caller (headers are sent before the SSE body,
            // so this works for streams): the header names what ACTUALLY served,
            // including recovery after the judge's selection.
            stamp_routing_headers(
                res.headers_mut(),
                &original_model,
                &used_model,
                fallback_model.as_deref(),
            );
            res
        }
        Err(e) => {
            let duration_ms = start.elapsed().as_millis() as u64;
            tracing::error!(error = %e, "Chat completion stream backend error");

            state
                .debug_log
                .push(DebugCompletionEntry {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    model: original_model,
                    message_count: debug_message_count,
                    streaming: true,
                    status: "error".to_string(),
                    duration_ms,
                    usage: None,
                    error: Some(e.to_string()),
                })
                .await;

            // The stream never opened, so no 200/SSE bytes have been sent yet.
            // Surface the upstream failure as a real non-2xx status (preserving
            // a provider 429 and its rate-limit/retry headers) so clients can
            // catch it on the `!response.ok` path rather than having to parse an
            // in-band SSE error event. (Mid-stream failures, after the 200 is
            // committed, still arrive as in-band SSE error events — see the
            // tapped-stream error branch above.)
            crate::handlers::backend_error_response(&e)
        }
    }
}

/// Passthrough tap over the upstream SSE byte stream that carries the
/// `chat.completion` span for the life of the stream.
///
/// The handler future finishes — and exits the span for the last time — as
/// soon as the 200/SSE response is handed back, long before the model finishes
/// generating. tracing-opentelemetry stamps the exported end time on every
/// span *exit*, so if nothing re-enters the span while the body streams, the
/// span records only the time-to-open-stream (milliseconds) instead of the
/// full generation. Entering the span on every body poll keeps the recorded
/// end time advancing until the stream completes.
///
/// While chunks pass through, the tap also:
/// - accumulates each choice's `delta` fragments and stamps the assembled
///   output onto the span at stream end (`llm.output_messages.*` /
///   `output.value`, matching the non-streaming path), and
/// - extracts `usage` from the final chunk for token-count attributes and
///   usage tracking, attributed to the resolved upstream connection.
///
/// SSE events may split across network chunks, so frames are decoded
/// incrementally via `SseDecoder` rather than parsed off of each raw chunk.
struct SseSpanTap {
    inner: SseByteStream,
    span: LlmSpanWriter,
    /// Decodes complete SSE frames from the upstream byte stream, so a
    /// multibyte character or JSON payload split across two network chunks
    /// is never misparsed (see `process_frame`).
    decoder: SseDecoder,
    /// Accumulated output, keyed by choice index (then tool-call index).
    choices: BTreeMap<i32, ChoiceAccumulator>,
    /// Latest cumulative usage snapshot seen in the stream. Providers may
    /// repeat usage on multiple chunks; bookkeeping runs once from `finalize`.
    /// Also stored with the cached entry, so a later hit reports the same token
    /// counts the live response did.
    usage: Option<Usage>,
    finalized: bool,
    state: crate::AppState,
    usage_attribution: UsageAttribution,
    /// Request-handling start, for `llm.time_to_first_token_ms`.
    start: std::time::Instant,
    /// Set once the first content-bearing (or tool-call-bearing) delta has been
    /// seen, so the time-to-first-token attribute is stamped exactly once and
    /// never on the role-only announcement chunk (or an empty-content delta —
    /// OpenAI commonly sends `"content":""` alongside the role announcement).
    first_token_seen: bool,
    estimate: Option<UsageEstimateState>,
    pending_write: Option<UsageWriteFuture>,
    /// Admission grant held until the stream completes or is dropped. Only a
    /// clean completion reconciles the conservative token reservation; an
    /// incomplete stream retains it while still releasing concurrency.
    rate_limit_permit: Option<QuotaPermit>,
    pending_terminal: Option<bytes::Bytes>,
    end_after_write: bool,
    terminated: bool,
    /// Whether to accumulate message content / tool-call-argument bytes at all.
    ///
    /// Resolved once at construction rather than re-checked per delta. Two
    /// consumers want the assembled text — the span and the prompt cache — and
    /// only the span is gated by `ROUTER_TRACE_LOG_CONTENT`, so accumulation
    /// happens when *either* does. (`set_output_span_attributes` re-checks the
    /// content-logging setting itself, so accumulating for the cache alone does
    /// not leak content onto a metadata-only span.) With neither consumer, a
    /// stream buffers nothing — which is the point of the gate: otherwise every
    /// concurrent stream holds its full generation in memory for nothing.
    capture_content: bool,
    /// Cache slot for this request, when the key opted in. `None` disables all
    /// of the store-side work below.
    cache_ctx: Option<CacheContext>,
    /// Public model id that actually served, recorded on the cached entry.
    served_model: String,
    /// Response identity echoed from the chunks, so the completion assembled
    /// for the cache carries the upstream's own id/model/timestamp rather than
    /// invented ones.
    response_id: Option<String>,
    response_model: Option<String>,
    response_created: Option<i64>,
    /// Running total of bytes pushed into `choices` toward the entry that
    /// would be cached (content plus tool-call arguments across all
    /// choices). Only tracked when `cache_ctx` is set.
    cache_len: usize,
    /// Set once the assembled entry would exceed `cache_ctx`'s ceiling.
    /// `store_in_cache` then refuses to write it, mirroring
    /// `AnthropicSseSpanTap::cache_overflowed`: a cache is not worth holding
    /// an unbounded generation in memory for.
    cache_overflowed: bool,
    /// Set when the upstream stream ended in a state that means "this
    /// completion is whole": a `[DONE]` marker, or a clean end-of-stream. The
    /// error branch and a client disconnect leave it false, and only a whole
    /// completion is ever cached — half an answer must never be replayed as a
    /// full one.
    clean_end: bool,
}

#[derive(Default)]
struct ChoiceAccumulator {
    role: Option<String>,
    /// Left empty when `log_content` is false.
    content: String,
    tool_calls: BTreeMap<i32, ToolCallAccumulator>,
    finish_reason: Option<String>,
}

#[derive(Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    /// Left empty when `log_content` is false.
    arguments: String,
}

impl SseSpanTap {
    #[allow(clippy::too_many_arguments)]
    fn new(
        inner: SseByteStream,
        span: LlmSpanWriter,
        state: crate::AppState,
        api_key: &ResolvedApiKey,
        resolution: &RouteResolution,
        start: std::time::Instant,
        normalized_usage_input: Option<NormalizedUsageInput>,
        cache_ctx: Option<CacheContext>,
        served_model: String,
        rate_limit_permit: Option<QuotaPermit>,
    ) -> Self {
        // Estimation consumes deltas incrementally. Only logging, caching, or
        // arbitrary outcome selectors need to retain the generated prose.
        // Resolve once: policy edits do not retroactively enable capture.
        let capture_content = super::content_logging_enabled()
            || cache_ctx.is_some()
            || state.sampling_gate.needs_chat_outcome_content();
        Self {
            inner,
            span,
            decoder: SseDecoder::default(),
            choices: BTreeMap::new(),
            usage: None,
            finalized: false,
            usage_attribution: UsageAttribution::new(api_key, resolution),
            state,
            start,
            first_token_seen: false,
            estimate: UsageEstimateState::new(resolution, normalized_usage_input),
            pending_write: None,
            rate_limit_permit,
            pending_terminal: None,
            end_after_write: false,
            terminated: false,
            capture_content,
            cache_ctx,
            served_model,
            response_id: None,
            response_model: None,
            response_created: None,
            cache_len: 0,
            cache_overflowed: false,
            clean_end: false,
        }
    }

    /// Add `added` bytes to the running cache-entry size estimate, capping it
    /// at `max_entry_bytes`. Once the ceiling is passed, `*overflowed`
    /// latches permanently for this stream. Mirrors
    /// `AnthropicSseSpanTap::buffer_for_cache`'s cap-then-abandon behavior,
    /// adapted to chat.rs's accumulator (rather than raw-byte-buffer) model.
    fn grow_cache_len(
        cache_len: &mut usize,
        overflowed: &mut bool,
        max_entry_bytes: usize,
        added: usize,
    ) {
        if *overflowed || added == 0 {
            return;
        }
        if *cache_len + added > max_entry_bytes {
            *overflowed = true;
            tracing::debug!(
                max_entry_bytes,
                "prompt cache: streamed chat completion exceeds the entry ceiling; not cached"
            );
            return;
        }
        *cache_len += added;
    }

    /// Decode the complete SSE frames now available and observe each in turn.
    fn process_chunk(&mut self, bytes: &bytes::Bytes) {
        for frame in self.decoder.push(bytes) {
            self.process_frame(frame);
        }
    }

    fn process_frame(&mut self, frame: SseFrame) {
        if frame.data == "[DONE]" {
            self.clean_end = true;
            self.finalize();
            return;
        }
        if self.cache_ctx.is_some() {
            let shape = serde_json::from_str::<serde_json::Value>(&frame.data).ok();
            if !shape
                .as_ref()
                .is_some_and(|value| chat_cache_shape_is_supported(value, true))
            {
                self.cache_ctx = None;
            }
        }
        let Ok(chunk) = serde_json::from_str::<ChatCompletionChunk>(&frame.data) else {
            self.cache_ctx = None;
            return;
        };
        // Echo the upstream's own response identity onto the assembled
        // completion (first chunk wins; every chunk of one response repeats it).
        if self.response_id.is_none() {
            self.response_id = Some(chunk.id.clone());
            self.response_model = Some(chunk.model.clone());
            self.response_created = Some(chunk.created);
        }
        // First content-bearing (or tool-call-bearing) delta across any choice
        // marks time-to-first-token; the role-only announcement chunk must NOT
        // count, so this is gated on non-empty content/tool_calls, not mere
        // delta presence — OpenAI commonly sends `"content":""` alongside the
        // role announcement, which must not count either.
        if !self.first_token_seen
            && chunk.choices.iter().any(|c| {
                c.delta.content.as_ref().is_some_and(|c| !c.is_empty())
                    || c.delta.tool_calls.as_ref().is_some_and(|t| !t.is_empty())
            })
        {
            self.first_token_seen = true;
            self.span
                .set_time_to_first_token_ms(self.start.elapsed().as_millis() as i64);
        }
        for choice in &chunk.choices {
            let acc = self.choices.entry(choice.index).or_default();
            if let Some(ref role) = choice.delta.role {
                acc.role = Some(role.clone());
            }
            if let Some(ref content) = choice.delta.content {
                if let Some(estimate) = self.estimate.as_mut() {
                    estimate.observe_text(content);
                }
                if self.capture_content {
                    acc.content.push_str(content);
                    if let Some(ctx) = self.cache_ctx.as_ref() {
                        Self::grow_cache_len(
                            &mut self.cache_len,
                            &mut self.cache_overflowed,
                            ctx.max_entry_bytes,
                            content.len(),
                        );
                    }
                }
            }
            if let Some(ref tool_calls) = choice.delta.tool_calls {
                for tc in tool_calls {
                    let tc_acc = acc.tool_calls.entry(tc.index).or_default();
                    if let Some(ref id) = tc.id {
                        tc_acc.id = id.clone();
                    }
                    if let Some(ref function) = tc.function {
                        if let Some(ref name) = function.name {
                            if let Some(estimate) = self.estimate.as_mut() {
                                estimate.observe_text(name);
                            }
                            tc_acc.name = name.clone();
                        }
                        if let Some(ref args) = function.arguments {
                            if let Some(estimate) = self.estimate.as_mut() {
                                estimate.observe_text(args);
                            }
                            if self.capture_content {
                                tc_acc.arguments.push_str(args);
                                if let Some(ctx) = self.cache_ctx.as_ref() {
                                    Self::grow_cache_len(
                                        &mut self.cache_len,
                                        &mut self.cache_overflowed,
                                        ctx.max_entry_bytes,
                                        args.len(),
                                    );
                                }
                            }
                        }
                    }
                }
            }
            if let Some(ref finish) = choice.finish_reason {
                acc.finish_reason = Some(finish.clone());
            }
        }
        if let Some(ref usage) = chunk.usage {
            // Kept for the cached entry too, so a hit reports the same token
            // counts the live stream did. `record_usage` runs once in
            // `finalize()` from `self.usage.take()`.
            self.usage = Some(usage.clone());
        }
    }

    /// Record usage from the final chunk: token-count span attributes plus the
    /// per-key / usage-tracker bookkeeping.
    fn record_usage(&mut self, usage: TokenUsage) {
        usage.stamp(&self.span);
        let write =
            super::usage::record_future(self.state.clone(), self.usage_attribution.clone(), usage);
        let actual_amount = self.clean_end.then_some(usage.total_tokens);
        self.pending_write = Some(super::usage::with_quota_settlement(
            write,
            self.rate_limit_permit.take(),
            actual_amount,
        ));
    }

    /// Stamp the assembled output messages onto the span, and — for a stream
    /// that ended whole — store the assembled completion in the prompt cache.
    ///
    /// Runs once, at `[DONE]`, stream end, or drop (client disconnect),
    /// whichever comes first, so a severed stream still records what was
    /// generated so far. Recording and caching diverge exactly there: a partial
    /// generation is worth putting on the span (it is what happened) and must
    /// never be cached (it would be replayed to the next caller as a complete
    /// answer). `clean_end` is the gate.
    fn finalize(&mut self) {
        if self.finalized {
            return;
        }
        self.finalized = true;
        let reported = self.usage.take().as_ref().and_then(TokenUsage::from_chat);
        let usage = reported.or_else(|| self.estimate.take().map(TokenUsage::from_estimate));
        if let Some(usage) = usage {
            self.record_usage(usage);
        } else {
            self.pending_write = Some(super::usage::with_quota_settlement(
                super::usage::missing_usage_future(),
                self.rate_limit_permit.take(),
                None,
            ));
        }
        if self.choices.is_empty() {
            return;
        }
        let choices: Vec<ChatChoice> = self
            .choices
            .iter()
            .map(|(&index, acc)| ChatChoice {
                index,
                message: ChatMessage {
                    role: acc.role.clone().unwrap_or_else(|| "assistant".to_string()),
                    content: (!acc.content.is_empty())
                        .then(|| serde_json::Value::String(acc.content.clone())),
                    name: None,
                    tool_calls: (!acc.tool_calls.is_empty()).then(|| {
                        acc.tool_calls
                            .values()
                            .map(|tc| ToolCall {
                                id: tc.id.clone(),
                                tool_type: "function".to_string(),
                                function: FunctionCall {
                                    name: tc.name.clone(),
                                    arguments: tc.arguments.clone(),
                                },
                            })
                            .collect()
                    }),
                    tool_call_id: None,
                },
                finish_reason: acc.finish_reason.clone(),
            })
            .collect();
        set_output_span_attributes(&self.span, &choices);
        self.span
            .apply_chat_outcome_sampling(&self.state.sampling_gate, &choices, None);
        self.store_in_cache(choices);
    }

    /// Store the completion assembled from this stream, if the request is
    /// cacheable and the stream ended whole.
    ///
    /// The stored value is the same canonical `ChatCompletionResponse` shape
    /// the non-streaming path stores, which is what lets a streamed miss serve
    /// a later non-streamed hit (and vice versa). A stream that produced no
    /// `finish_reason` on any choice is treated as unfinished even if the
    /// transport closed cleanly — some upstreams drop the connection without an
    /// error once they stop generating.
    fn store_in_cache(&mut self, choices: Vec<ChatChoice>) {
        let Some(ctx) = self.cache_ctx.as_ref() else {
            return;
        };
        if self.cache_overflowed {
            return;
        }
        if !self.clean_end {
            tracing::debug!("prompt cache: stream did not end cleanly; not cached");
            return;
        }
        if !choices.iter().any(|c| c.finish_reason.is_some()) {
            tracing::debug!("prompt cache: stream carried no finish_reason; not cached");
            return;
        }
        let response = ChatCompletionResponse {
            id: self.response_id.clone().unwrap_or_default(),
            object: "chat.completion".to_string(),
            created: self.response_created.unwrap_or_default(),
            model: self
                .response_model
                .clone()
                .unwrap_or_else(|| self.served_model.clone()),
            choices,
            usage: self.usage.clone(),
        };
        match serde_json::to_vec(&response) {
            Ok(payload) => crate::handlers::store_completion(
                &self.state,
                ctx,
                payload,
                PayloadKind::Json,
                self.served_model.clone(),
            ),
            Err(e) => {
                tracing::warn!(error = %e, "prompt cache: streamed completion did not serialize; not cached")
            }
        }
    }

    fn poll_usage_write(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Option<Poll<Option<Result<bytes::Bytes, std::convert::Infallible>>>> {
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
                tracing::error!(error = %error, "Chat stream usage could not be persisted");
                self.pending_write = None;
                self.pending_terminal = None;
                self.terminated = true;
                self.span
                    .set_error("usage_accounting_unavailable", Some(&error.to_string()));
                Some(Poll::Ready(Some(Ok(sse_error_and_done(&ErrorResponse {
                    error: Error {
                        message: "Usage accounting is temporarily unavailable".to_string(),
                        error_type: "server_error".to_string(),
                        param: None,
                        code: Some("usage_accounting_unavailable".to_string()),
                    },
                })))))
            }
        }
    }
}

/// Serialize an `ErrorResponse` as an unnamed `data:` SSE frame followed by
/// the `[DONE]` sentinel, both in one chunk — the shape every OpenAI-style
/// streaming error this tap emits has always used, so a client watching for
/// `[DONE]` still sees the stream end cleanly after the error.
fn sse_error_and_done(response: &ErrorResponse) -> bytes::Bytes {
    let json = serde_json::to_string(response).unwrap_or_else(|_| {
        "{\"error\":{\"message\":\"internal error\",\"type\":\"server_error\"}}".to_string()
    });
    let mut buf = encode_frame(None, &json).to_vec();
    buf.extend_from_slice(&done_frame());
    bytes::Bytes::from(buf)
}

impl Stream for SseSpanTap {
    type Item = Result<bytes::Bytes, std::convert::Infallible>;

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
                this.process_chunk(&bytes);
                if this.pending_write.is_some() {
                    this.pending_terminal = Some(bytes);
                    this.poll_usage_write(cx).unwrap_or(Poll::Pending)
                } else {
                    Poll::Ready(Some(Ok(bytes)))
                }
            }
            Poll::Ready(Some(Err(e))) => {
                tracing::error!(error = %e, "Stream error from backend");
                this.finalize();
                if let Some(future) = this.pending_write.take() {
                    super::usage::detach_future(future);
                }
                this.span.set_error("backend_error", Some(&e.to_string()));
                // Convert error to an SSE error event
                Poll::Ready(Some(Ok(sse_error_and_done(&super::backend_error_body(&e)))))
            }
            Poll::Ready(None) => {
                // End of stream with no transport error: the generation is
                // whole even if the upstream omitted the `[DONE]` marker.
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

impl Drop for SseSpanTap {
    fn drop(&mut self) {
        // A client disconnect drops the body mid-stream: record what was
        // accumulated and stamp a final end time on the way out.
        let span = self.span.clone();
        let _enter = span.enter();
        self.finalize();
        if let Some(future) = self.pending_write.take() {
            super::usage::detach_future(future);
        }
    }
}

/// Handle non-streaming chat completions with fallback support.
async fn handle_non_streaming(
    current_span: LlmSpanWriter,
    state: crate::AppState,
    api_key: ResolvedApiKey,
    req: NativeChatRequest,
    original_model: String,
    debug_message_count: usize,
    start: std::time::Instant,
) -> Response {
    let normalized_usage_input = req.normalized_usage_input();
    // Opaque parts leave admission a soft cap over the visible input.
    let admission_tokens = normalized_usage_input
        .or_else(|| req.soft_cap_admission_input())
        .map(|input| input.conservative_tokens());
    // Resolve the route EXACTLY ONCE. This resolution is carried through the
    // permission gate, dispatch, and (on success) usage attribution — the
    // snapshot can't shift the request between connections mid-flight. A resolve
    // error is terminal here (surfaced as the routing error) so a restricted key
    // whose model won't resolve is never dispatched unchecked.
    let route = match state.backend.resolve(&original_model).await {
        Ok(r) => r,
        Err(e) => {
            let duration_ms = start.elapsed().as_millis() as u64;
            tracing::error!(error = %e, "Chat completion routing error");
            state
                .debug_log
                .push(DebugCompletionEntry {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    model: original_model,
                    message_count: debug_message_count,
                    streaming: false,
                    status: "error".to_string(),
                    duration_ms,
                    usage: None,
                    error: Some(e.to_string()),
                })
                .await;
            return crate::handlers::backend_error_response(&e);
        }
    };

    // Stamp the span from the primary resolution NOW — before the gate and
    // dispatch — so a denied/errored span still carries system/model/connection.
    // (Re-stamped below from the WINNING resolution when a fallback serves the
    // request.)
    set_route_span_attributes(&current_span, &route.resolution);
    // OpenInference-adjacent: distinguish streamed from non-streamed spans.
    current_span.set_stream(false);

    // Enforce the key's provider/model allowlist on the PRIMARY before dispatch.
    // A denial here is terminal (403) — we do not fall back to alternates the key
    // also isn't permitted to reach.
    if api_key.0.permission.is_restricted() {
        if let Err(denied) = api_key
            .0
            .permission
            .allows_route(&route.resolution, &original_model)
        {
            tracing::warn!(
                key_prefix = %api_key.0.key_prefix,
                model = %original_model,
                reason = %denied,
                "Chat completion denied by API key permission"
            );
            state
                .debug_log
                .push(DebugCompletionEntry {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    model: original_model,
                    message_count: debug_message_count,
                    streaming: false,
                    status: "error".to_string(),
                    duration_ms: start.elapsed().as_millis() as u64,
                    usage: None,
                    error: Some(denied.to_string()),
                })
                .await;
            return crate::handlers::forbidden_response(denied.to_string());
        }
    }

    // Exact-prefix cache: consulted here, between the permission gate and the
    // judge (see the cache section above for why that ordering matters).
    let cache_ctx = if route.routing.is_none() && req.fallbacks.is_empty() {
        state.native_chat_cache_context(&api_key.0, &req)
    } else {
        None
    };
    if let Some(ctx) = &cache_ctx {
        if let Some(value) = state.prompt_cache.get(&ctx.key).await {
            if let Some(res) = cache_hit_response(
                &current_span,
                &state.sampling_gate,
                state.prompt_cache.kind(),
                ctx,
                &value,
                &req,
                &original_model,
            ) {
                let cache_permit = match super::routing::acquire_request_quota(
                    &state,
                    &api_key,
                    TokenReservation::NotApplicable,
                )
                .await
                {
                    Ok(permit) => permit,
                    Err(error) => return crate::handlers::backend_error_response(&error),
                };
                if let Some(permit) = cache_permit {
                    permit.finish(None).await;
                }
                state
                    .debug_log
                    .push(DebugCompletionEntry {
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        model: value.served_model.clone(),
                        message_count: debug_message_count,
                        streaming: false,
                        status: "cached".to_string(),
                        duration_ms: start.elapsed().as_millis() as u64,
                        usage: None,
                        error: None,
                    })
                    .await;
                return res;
            }
        }
    }

    let selection = match crate::judge::routing::select(
        &state,
        &api_key,
        &original_model,
        &req.fallbacks,
        route,
        || crate::judge::input::JudgeInput::chat(&req),
        &current_span,
    )
    .await
    {
        Ok(selection) => selection,
        Err(error) => return super::backend_error_response(&error),
    };
    let served_model = selection.plan.primary_model.clone();

    let result = try_with_fallbacks(
        &current_span,
        &state,
        req,
        selection.plan,
        &api_key,
        admission_tokens,
        dispatch_completion,
    )
    .await;

    if result.is_ok() {
        crate::judge::routing::spawn_shadow_if_enabled(
            state.clone(),
            api_key.clone(),
            selection.shadow,
            current_span.clone(),
        );
    }
    match result {
        Ok((upstream, fallback_model, resolution, rate_limit_permit)) => {
            let BackendChatResponse {
                body: native_body,
                completion: response,
                status: upstream_status,
                content_type,
                headers: upstream_headers,
            } = upstream;
            let duration_ms = start.elapsed().as_millis() as u64;

            // Attribute usage/span to the WINNING route's resolution (returned by
            // `try_with_fallbacks` — never re-resolved). A fallback may differ
            // from the primary, so re-stamp the span to reflect what actually
            // served the request.
            let used_model = fallback_model
                .as_deref()
                .unwrap_or(&served_model)
                .to_string();
            set_route_span_attributes(&current_span, &resolution);
            // `chalk.router.served_model` must name what ACTUALLY served: after
            // a downgrade, a fallback may have descended below the rung the
            // judge selected.
            if used_model != original_model {
                current_span.set_requested_served_models(&original_model, &used_model);
            }

            // Provider-reported usage wins. A built-in text adapter that omits
            // or malforms it falls back to the conservative estimator.
            let reported = response.usage.as_ref().and_then(TokenUsage::from_chat);
            let estimated = || {
                let mut estimate = UsageEstimateState::new(&resolution, normalized_usage_input)?;
                estimate.observe_json(&serde_json::to_value(&response.choices).ok()?);
                Some(TokenUsage::from_estimate(estimate))
            };
            let Some(usage) = reported.or_else(estimated) else {
                return super::usage_write_error_response();
            };
            usage.stamp(&current_span);
            if super::usage::with_quota_settlement(
                super::usage::record_future(
                    state.clone(),
                    UsageAttribution::new(&api_key, &resolution),
                    usage,
                ),
                rate_limit_permit,
                Some(usage.total_tokens),
            )
            .await
            .is_err()
            {
                return super::usage_write_error_response();
            }

            // Expand output only after fixed result metadata so a wide set of
            // tool calls cannot evict token counts under the SDK's first-N cap.
            set_output_span_attributes(&current_span, &response.choices);
            current_span.record_json_str(ContentDirection::Output, native_body.get());
            current_span.apply_chat_outcome_sampling(
                &state.sampling_gate,
                &response.choices,
                Some(&response),
            );

            state
                .debug_log
                .push(DebugCompletionEntry {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    model: used_model.to_string(),
                    message_count: debug_message_count,
                    streaming: false,
                    status: "ok".to_string(),
                    duration_ms,
                    usage: response.usage.clone(),
                    error: None,
                })
                .await;

            // Adopt the raw JSON allocation for delivery without reserializing
            // or copying it. The cache keeps its existing owned byte payload.
            let native_body: Box<str> = native_body.into();
            let native_body = Bytes::from(String::from(native_body));
            let cache_payload = cache_ctx
                .as_ref()
                .filter(|_| upstream_status == 200 && content_type == "application/json")
                .map(|_| native_body.to_vec());

            let mut res = Response::builder()
                .status(StatusCode::from_u16(upstream_status).unwrap_or(StatusCode::OK))
                .header(header::CONTENT_TYPE, content_type)
                .body(Body::from(native_body))
                .expect("native Chat response");
            crate::handlers::apply_forwarded_headers(res.headers_mut(), &upstream_headers);
            crate::handlers::stamp_trace_headers(res.headers_mut());
            if let Some(ctx) = &cache_ctx {
                crate::handlers::stamp_cache_outcome(
                    &current_span,
                    Some(res.headers_mut()),
                    state.prompt_cache.kind(),
                    ctx,
                    None,
                );
                match cache_payload {
                    Some(payload) => crate::handlers::store_completion(
                        &state,
                        ctx,
                        payload,
                        PayloadKind::Json,
                        used_model.clone(),
                    ),
                    None => {
                        tracing::debug!("prompt cache: native response status or content type cannot be replayed")
                    }
                }
            }
            // Surface routing to the caller: the header names what ACTUALLY
            // served (a fallback may differ from the judge's pick).
            stamp_routing_headers(
                res.headers_mut(),
                &original_model,
                &used_model,
                fallback_model.as_deref(),
            );
            res
        }
        Err(e) => {
            let duration_ms = start.elapsed().as_millis() as u64;

            tracing::error!(error = %e, "Chat completion backend error");

            state
                .debug_log
                .push(DebugCompletionEntry {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    model: original_model,
                    message_count: debug_message_count,
                    streaming: false,
                    status: "error".to_string(),
                    duration_ms,
                    usage: None,
                    error: Some(e.to_string()),
                })
                .await;
            // Preserve the upstream status (e.g. a provider 429) and forward
            // rate-limit / retry headers instead of collapsing to 500.
            crate::handlers::backend_error_response(&e)
        }
    }
}

/// Convert a `ChatCompletionResponse` into the SSE wire form.
///
/// This is how a streaming request is answered from the cache: the stored value
/// is always the canonical completion, so serving it to a streaming caller
/// means re-emitting it as the chunk sequence a live upstream would have sent —
/// role announcement, content, tool calls, a final chunk carrying
/// `finish_reason` and `usage`, then `[DONE]`. The chunk *boundaries* differ
/// from a live stream (there is nothing to interleave, the whole answer is
/// already here), but every event a client parses is the same shape.
fn create_sse_response(response: ChatCompletionResponse) -> Response {
    let mut events: Vec<bytes::Bytes> = Vec::new();

    // First chunk: role announcement
    let role_chunk = ChatCompletionChunk {
        id: response.id.clone(),
        object: "chat.completion.chunk".to_string(),
        created: response.created,
        model: response.model.clone(),
        choices: response
            .choices
            .iter()
            .map(|choice| ChatCompletionChunkChoice {
                index: choice.index,
                delta: ChatCompletionDelta {
                    role: Some(choice.message.role.clone()),
                    content: None,
                    tool_calls: None,
                },
                finish_reason: None,
            })
            .collect(),
        usage: None,
    };

    if let Ok(json) = serde_json::to_string(&role_chunk) {
        events.push(encode_frame(None, &json));
    }

    // Content chunks - send the content in the response
    for choice in &response.choices {
        let content = message_content_to_string(&choice.message.content);

        if let Some(text) = content {
            let content_chunk = ChatCompletionChunk {
                id: response.id.clone(),
                object: "chat.completion.chunk".to_string(),
                created: response.created,
                model: response.model.clone(),
                choices: vec![ChatCompletionChunkChoice {
                    index: choice.index,
                    delta: ChatCompletionDelta {
                        role: None,
                        content: Some(text),
                        tool_calls: None,
                    },
                    finish_reason: None,
                }],
                usage: None,
            };

            if let Ok(json) = serde_json::to_string(&content_chunk) {
                events.push(encode_frame(None, &json));
            }
        }

        // Handle tool calls if present
        if let Some(tool_calls) = &choice.message.tool_calls {
            for (idx, tool_call) in tool_calls.iter().enumerate() {
                let tool_chunk = ChatCompletionChunk {
                    id: response.id.clone(),
                    object: "chat.completion.chunk".to_string(),
                    created: response.created,
                    model: response.model.clone(),
                    choices: vec![ChatCompletionChunkChoice {
                        index: choice.index,
                        delta: ChatCompletionDelta {
                            role: None,
                            content: None,
                            tool_calls: Some(vec![crate::types::ToolCallDelta {
                                index: idx as i32,
                                id: Some(tool_call.id.clone()),
                                tool_type: Some(tool_call.tool_type.clone()),
                                function: Some(crate::types::FunctionCallDelta {
                                    name: Some(tool_call.function.name.clone()),
                                    arguments: Some(tool_call.function.arguments.clone()),
                                }),
                            }]),
                        },
                        finish_reason: None,
                    }],
                    usage: None,
                };

                if let Ok(json) = serde_json::to_string(&tool_chunk) {
                    events.push(encode_frame(None, &json));
                }
            }
        }
    }

    // Final chunk with finish_reason and usage
    let final_chunk = ChatCompletionChunk {
        id: response.id.clone(),
        object: "chat.completion.chunk".to_string(),
        created: response.created,
        model: response.model.clone(),
        choices: response
            .choices
            .iter()
            .map(|choice| ChatCompletionChunkChoice {
                index: choice.index,
                delta: ChatCompletionDelta {
                    role: None,
                    content: None,
                    tool_calls: None,
                },
                finish_reason: choice.finish_reason.clone(),
            })
            .collect(),
        usage: response.usage.clone(),
    };

    if let Ok(json) = serde_json::to_string(&final_chunk) {
        events.push(encode_frame(None, &json));
    }

    // Add the [DONE] marker
    events.push(done_frame());

    // Create streaming body
    let body_stream = stream::iter(events.into_iter().map(Ok::<_, std::convert::Infallible>));
    let body = Body::from_stream(body_stream);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(body)
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
    use opentelemetry_sdk::trace::{SimpleSpanProcessor, TracerProvider};
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn invocation_parameters_compact_retained_schema_without_losing_scalar_controls() {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("schema-order-test")));
        let wire = format!(
            r#"{{"model":"mock","messages":[],"temperature":0.25,"top_p":0.75,"max_tokens":64,"response_format":{{{}"type":"json_schema","json_schema":{{"schema":{{"properties":{{"z":{{"properties":{{"yy":{{}},"bb":{{}}}}}},"a":{{"minimum":1.2300e-4}}}}}}}}}}}}"#,
            " ".repeat(MAX_INVOCATION_PARAMETERS_BYTES + 1)
        );
        assert!(wire.len() > MAX_INVOCATION_PARAMETERS_BYTES);
        let request: NativeChatRequest = serde_json::from_str(&wire).unwrap();
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let span = LlmSpanWriter::new(
                tracing::info_span!("ordered_invocation"),
                None,
                LlmSpanKind::Llm,
            );
            set_input_span_attributes(&span, &request, true);
        }
        for result in provider.force_flush() {
            result.unwrap();
        }
        let spans = exporter.get_finished_spans().unwrap();
        let span = spans
            .iter()
            .find(|span| span.name.as_ref() == "ordered_invocation")
            .unwrap();
        let parameters = span
            .attributes
            .iter()
            .find(|attribute| attribute.key.as_str() == "llm.invocation_parameters")
            .unwrap()
            .value
            .to_string();
        assert!(
            parameters.contains(
                r#""properties":{"z":{"properties":{"yy":{},"bb":{}}},"a":{"minimum":1.2300e-4}}"#
            ),
            "{parameters}"
        );
        assert!(parameters.len() < MAX_INVOCATION_PARAMETERS_BYTES);
        let value: serde_json::Value = serde_json::from_str(&parameters).unwrap();
        assert_eq!(value["temperature"], 0.25);
        assert_eq!(value["top_p"], 0.75);
        assert_eq!(value["max_tokens"], 64);
        assert_eq!(value["response_format"]["type"], "json_schema");
    }

    #[tokio::test]
    async fn oversized_content_uploads_when_object_storage_is_configured() {
        crate::handlers::init_content_logging(true);
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let telemetry_layer =
            tracing_opentelemetry::layer().with_tracer(provider.tracer("oversized-content-test"));
        let subscriber = tracing_subscriber::registry().with(telemetry_layer);
        let object_store = Some(Arc::new(crate::storage::ObjectStore::memory()));

        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let span = tracing::info_span!("oversized_content");
            let span = LlmSpanWriter::new(span, object_store, LlmSpanKind::Llm);
            let content = span
                .record_json_string(
                    ContentDirection::Input,
                    "x".repeat(crate::handlers::trace_attributes::MAX_INLINE_CONTENT_BYTES + 1),
                )
                .unwrap();
            assert_eq!(content.inline_bytes, 0);
            content.upload.unwrap().await.unwrap();
            drop(span);
        }

        for result in provider.force_flush() {
            result.unwrap();
        }
        let spans = exporter.get_finished_spans().unwrap();
        let span = spans
            .iter()
            .find(|span| span.name.as_ref() == "oversized_content")
            .expect("oversized content span should be exported");
        let attributes: std::collections::HashMap<_, _> = span
            .attributes
            .iter()
            .map(|attribute| (attribute.key.to_string(), attribute.value.to_string()))
            .collect();

        assert!(!attributes.contains_key("input.value"));
        assert!(
            attributes
                .get("chalk.trace.input.content_uri")
                .is_some_and(|uri| uri.starts_with("memory://") && uri.ends_with(".json")),
            "oversized JSON should be uploaded: {attributes:?}"
        );
        assert!(!attributes.contains_key("chalk.trace.input.upload_omitted"));
    }

    #[test]
    fn metadata_only_mode_does_not_emit_tool_schemas() {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let telemetry_layer =
            tracing_opentelemetry::layer().with_tracer(provider.tracer("metadata-only-test"));
        let subscriber = tracing_subscriber::registry().with(telemetry_layer);
        let request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "mock-model",
            "messages": [{"role": "user", "content": "secret"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "private_tool",
                    "description": "private description",
                    "parameters": {"type": "object"}
                }
            }]
        }))
        .unwrap();

        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let span = tracing::info_span!("metadata_only");
            let (input, output) = crate::handlers::trace_attributes::llm_attribute_budgets();
            let span = LlmSpanWriter::new_with_limits(
                span,
                LlmSpanKind::Llm,
                input,
                output,
                crate::handlers::trace_attributes::MAX_LLM_SPAN_CONTENT_BYTES,
                false,
            );
            set_input_span_attributes(
                &span,
                &NativeChatRequest::from_typed(request).unwrap(),
                false,
            );
            drop(span);
        }
        for result in provider.force_flush() {
            result.unwrap();
        }
        let spans = exporter.get_finished_spans().unwrap();
        let span = spans
            .iter()
            .find(|span| span.name.as_ref() == "metadata_only")
            .unwrap();
        let attributes: std::collections::HashMap<_, _> = span
            .attributes
            .iter()
            .map(|attribute| (attribute.key.to_string(), attribute.value.to_string()))
            .collect();

        assert_eq!(
            attributes
                .get("llm.input_messages.0.message.role")
                .map(String::as_str),
            Some("user")
        );
        assert!(!attributes.contains_key("input.value"));
        assert!(!attributes.keys().any(|key| key.starts_with("llm.tools.")));
    }

    #[test]
    fn image_indices_are_assigned_before_fetch_or_upload() {
        let request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "mock-model",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image_url", "image_url": {"url": "https://example.com/first.png"}},
                    {"type": "text", "text": "between"},
                    {"type": "image_url", "image_url": {"url": "https://example.com/second.png"}}
                ]
            }]
        }))
        .unwrap();

        assert_eq!(
            indexed_image_urls(&request).collect::<Vec<_>>(),
            vec![
                (0, "https://example.com/first.png"),
                (1, "https://example.com/second.png")
            ]
        );
    }

    #[test]
    fn truncation_preserves_the_newest_message_and_reserves_tools() {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer().with_tracer(provider.tracer("input-priority-test")),
        );
        let mut messages = vec![serde_json::json!({
            "role": "system",
            "content": "keep this system prompt",
        })];
        messages.extend((0..50).map(|index| {
            serde_json::json!({
                "role": "user",
                "content": format!("message-{index}"),
            })
        }));
        let request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "mock-model",
            "messages": messages,
            "tools": (0..8).map(|index| serde_json::json!({
                "type": "function",
                "function": {
                    "name": format!("tool-{index}"),
                    "parameters": {"type": "object"},
                },
            })).collect::<Vec<_>>(),
        }))
        .unwrap();

        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let span = tracing::info_span!("input_priority");
            let (input, output) = crate::handlers::trace_attributes::llm_attribute_budgets();
            let span = LlmSpanWriter::new_with_limits(
                span,
                LlmSpanKind::Llm,
                input,
                output,
                crate::handlers::trace_attributes::MAX_LLM_SPAN_CONTENT_BYTES,
                true,
            );
            set_input_span_attributes(
                &span,
                &NativeChatRequest::from_typed(request).unwrap(),
                true,
            );
            drop(span);
        }

        for result in provider.force_flush() {
            result.unwrap();
        }
        let attributes: std::collections::HashMap<_, _> = exporter
            .get_finished_spans()
            .unwrap()
            .into_iter()
            .find(|span| span.name.as_ref() == "input_priority")
            .unwrap()
            .attributes
            .into_iter()
            .map(|attribute| (attribute.key.to_string(), attribute.value.to_string()))
            .collect();
        assert_eq!(
            attributes
                .get("llm.input_messages.50.message.content")
                .map(String::as_str),
            Some("message-49")
        );
        assert!(!attributes.contains_key("llm.input_messages.0.message.content"));
        assert!(attributes.contains_key("llm.tools.0.tool.json_schema"));
        assert!(attributes.contains_key("chalk.trace.input.truncation"));
    }
}

#[cfg(test)]
mod native_stream_visibility_tests {
    use super::filter_usage_chunks;
    use bytes::Bytes;
    use futures::{stream, StreamExt};

    #[tokio::test]
    async fn keepalive_is_forwarded_before_the_upstream_can_emit_data() {
        let (release, gated) = tokio::sync::oneshot::channel::<()>();
        let upstream = stream::iter(vec![Ok(Bytes::from_static(b": keepalive\r\n\r\n"))]).chain(
            stream::once(async move {
                gated.await.expect("release upstream");
                Ok(Bytes::from_static(b"data: [DONE]\n\n"))
            }),
        );
        let filtered = filter_usage_chunks(upstream, false);
        futures::pin_mut!(filtered);
        let heartbeat = tokio::time::timeout(std::time::Duration::from_secs(1), filtered.next())
            .await
            .expect("heartbeat must not wait for data")
            .unwrap()
            .unwrap();
        assert_eq!(heartbeat, ": keepalive\r\n\r\n");
        release.send(()).unwrap();
        assert_eq!(filtered.next().await.unwrap().unwrap(), "data: [DONE]\n\n");
        assert!(filtered.next().await.is_none());
    }

    #[tokio::test]
    async fn unterminated_usage_only_frame_is_hidden_at_eof() {
        let upstream = stream::iter(vec![Ok(Bytes::from_static(
            b"data: {\"choices\":[],\"usage\":{\"total_tokens\":1}}",
        ))]);
        let filtered = filter_usage_chunks(upstream, false);
        futures::pin_mut!(filtered);
        assert!(filtered.next().await.is_none());
    }
}
