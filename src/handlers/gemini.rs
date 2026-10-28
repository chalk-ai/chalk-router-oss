//! Gemini Interactions is a native surface: only the routed model changes.
//! Unknown fields, signed steps and SSE bytes remain intact. No router cache or
//! conversation store is involved. Background/agent jobs need a separate usage
//! and lifecycle contract and are explicitly unsupported here.

use crate::tenancy::RequestState as State;
use std::{
    collections::BTreeMap,
    pin::Pin,
    sync::Mutex,
    task::{Context, Poll},
};

use axum::{
    body::{Body, Bytes},
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use bytes::BytesMut;
use futures::Stream;
use serde_json::{json, value::RawValue, Value};

use super::{
    routing::{stamp_routing_headers, try_with_fallbacks_if, RoutePlan, RoutedRequest},
    sse::{encode_frame, SseDecoder},
    trace_attributes::{ContentDirection, LlmSpanKind, LlmSpanWriter},
    usage::{self, TokenUsage, UsageAttribution, UsageWriteFuture},
};
use crate::{
    backend::{
        provider::forwardable_upstream_headers, BackendError, BackendProvider, ResolvedRoute,
        RouteResolution,
    },
    capabilities::{DeliveryMode, Protocol, ProtocolOperation, Support},
    capability_requirements::gemini_interaction_requirements,
    middleware::ResolvedApiKey,
    quota::QuotaPermit,
    types::ErrorResponse,
    usage_estimator::NormalizedUsageInput,
};

#[derive(Clone)]
struct InteractionRequest {
    // Raw values prevent rewriting tool arguments, numbers, signatures, or
    // nested object/array ordering when replacing the top-level model.
    fields: BTreeMap<String, Box<RawValue>>,
    view: Value,
    headers: HeaderMap,
    input_tokens: Option<u64>,
}

impl InteractionRequest {
    fn parse(body: &[u8], headers: HeaderMap) -> Result<Self, BackendError> {
        let invalid = |message: &str| BackendError::api_status(400, message);
        let fields = serde_json::from_slice(body)
            .map_err(|_| invalid("Gemini request must be a JSON object"))?;
        let view: Value =
            serde_json::from_slice(body).map_err(|_| invalid("invalid Gemini request JSON"))?;
        if let Some(agent) = view.get("agent") {
            if !agent.as_str().is_some_and(|agent| !agent.trim().is_empty()) {
                return Err(invalid("Gemini `agent` must be a nonempty string"));
            }
        } else if !view["model"]
            .as_str()
            .is_some_and(|model| !model.trim().is_empty())
        {
            return Err(invalid("Gemini request requires a nonempty string `model` naming a router model or connection prefix"));
        }
        for field in ["stream", "store", "background"] {
            if view.get(field).is_some_and(|value| !value.is_boolean()) {
                return Err(invalid(&format!("Gemini `{field}` must be a boolean")));
            }
        }
        let mut usage_view = view.clone();
        if let Some(format) = usage_view.get_mut("response_format") {
            // Output declarations are text configuration, not media input. Keep their bytes in
            // the estimate without treating `type: image` as an unmeasurable input image.
            *format = Value::String(format.to_string());
        }
        let input_tokens =
            NormalizedUsageInput::from_json(&usage_view).map(|input| input.conservative_tokens());
        Ok(Self {
            fields,
            view,
            headers,
            input_tokens,
        })
    }

    fn streaming(&self) -> bool {
        self.view["stream"].as_bool() == Some(true)
    }

    fn may_fallback(&self) -> bool {
        // Google defaults store=true. A stored turn or an opaque continuation
        // must stay on the connection the client selected, even after a 5xx.
        self.view["store"].as_bool() == Some(false) && !has_bound_history(&self.view)
    }

    fn body(&self) -> Bytes {
        serde_json::to_vec(&self.fields)
            .expect("raw JSON fields serialize")
            .into()
    }
}

fn has_bound_history(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(has_bound_history),
        Value::Object(object) => {
            object.keys().any(|key| {
                matches!(
                    key.as_str(),
                    "previous_interaction_id"
                        | "signature"
                        | "thought_signature"
                        | "thoughtSignature"
                        | "cached_content"
                )
            }) || object
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| {
                    matches!(
                        kind,
                        "thought"
                            | "function_call"
                            | "function_result"
                            | "tool_call"
                            | "tool_result"
                            | "model_output"
                    )
                })
                || object.values().any(has_bound_history)
        }
        _ => false,
    }
}

impl RoutedRequest for InteractionRequest {
    fn set_model(&mut self, model: String) {
        let model = model.strip_prefix("models/").unwrap_or(&model);
        self.fields.insert(
            "model".into(),
            RawValue::from_string(serde_json::to_string(model).unwrap()).unwrap(),
        );
    }
    fn check_capability(&self, route: &ResolvedRoute) -> Result<(), String> {
        crate::capabilities::trace_decision(
            route,
            self.view["model"].as_str().unwrap_or_default(),
            &gemini_interaction_requirements(&self.view),
            "gemini_interactions.create",
        );
        (route.resolution.provider_kind == "gemini"
            && route
                .capabilities
                .profile
                .operation(Protocol::GeminiInteractions, ProtocolOperation::Create)
                .is_some_and(|operation| {
                    matches!(operation.mode, DeliveryMode::Native | DeliveryMode::Unknown)
                })
            && route
                .capabilities
                .profile
                .satisfies(&gemini_interaction_requirements(&self.view)))
        .then_some(())
        .ok_or_else(|| super::routing::NO_LOSSLESS_ROUTE.to_string())
    }
    fn capability_support(&self, route: &ResolvedRoute) -> Support {
        route
            .capabilities
            .profile
            .support_for(&gemini_interaction_requirements(&self.view))
    }
    fn conservative_input_tokens(&self) -> Option<u64> {
        self.input_tokens
    }
    fn rate_limit_tokens(
        &self,
        route: &ResolvedRoute,
        input: Option<u64>,
    ) -> (Option<u64>, Option<u64>) {
        let output = self.view["generation_config"]["max_output_tokens"]
            .as_u64()
            .or(route.capabilities.profile.limits.max_output_tokens);
        (
            input
                .zip(output)
                .map(|(input, output)| input.saturating_add(output)),
            input,
        )
    }
}

pub async fn unsupported_interaction_operation() -> Response {
    super::not_implemented("Gemini interaction retrieval, deletion and cancellation")
}

pub async fn create_interaction(
    State(state): State<crate::AppState>,
    api_key: ResolvedApiKey,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let span = LlmSpanWriter::new(
        tracing::info_span!("gemini_interactions", otel.name = "gemini.interactions"),
        state.object_store.clone(),
        LlmSpanKind::Llm,
    );
    super::stamp_authenticated_principal(&span, &api_key);
    span.clone()
        .instrument(create(&state, &api_key, headers, body, span))
        .await
}

async fn create(
    state: &crate::AppState,
    api_key: &ResolvedApiKey,
    headers: HeaderMap,
    body: Bytes,
    span: LlmSpanWriter,
) -> Response {
    let request = match InteractionRequest::parse(&body, headers) {
        Ok(request) => request,
        Err(error) => return super::backend_error_response(&error),
    };
    // Agent interactions name an agent instead of a model, so reject this
    // unsupported operation before model resolution. Auth has already run.
    if request.view.get("agent").is_some() {
        return super::not_implemented("Gemini agent interactions");
    }
    let model = request.view["model"].as_str().unwrap().to_string();
    // Strict resolution prevents an unknown Gemini model from reaching the
    // OpenAI catch-all. Clients use a saved connection prefix or a chalk route.
    let route = match state.backend.resolve_strict(&model).await {
        Ok(route) => route,
        Err(error) => return super::backend_error_response(&error),
    };
    span.set_route(&route.resolution);
    span.set_stream(request.streaming());
    stamp_content(&span, ContentDirection::Input, &request.view);
    if let Some(config) = request.view.get("generation_config") {
        span.set_invocation_parameters_json(config.to_string());
    }
    if let Err(denied) = api_key.0.permission.allows_route(&route.resolution, &model) {
        return super::forbidden_response(denied.to_string());
    }
    if request.view["background"].as_bool() == Some(true) {
        return super::not_implemented("Gemini background interactions");
    }
    let plan = RoutePlan::configured(model.clone(), route);
    let plan = if request.may_fallback() {
        plan
    } else {
        plan.without_fallbacks()
    };
    let raw_failure = Mutex::new(None);
    let input_tokens = request.conservative_input_tokens();
    let result = try_with_fallbacks_if(
        &span,
        state,
        request,
        plan,
        api_key,
        input_tokens,
        |route, request| {
            request
                .capability_eligible(route)
                .then_some(())
                .ok_or_else(|| {
                    "this request requires a native Gemini Interactions connection".to_string()
                })
        },
        |route, request| dispatch(route.backend.clone(), request, &raw_failure),
    )
    .await;
    let (upstream, fallback, resolution, permit) = match result {
        Ok(result) => result,
        Err(error) => {
            return raw_failure
                .into_inner()
                .unwrap()
                .unwrap_or_else(|| super::backend_error_response(&error))
        }
    };
    span.set_route(&resolution);
    let mut response = forward(upstream, state, api_key, &resolution, span, permit).await;
    super::stamp_trace_headers(response.headers_mut());
    stamp_routing_headers(
        response.headers_mut(),
        &model,
        fallback.as_deref().unwrap_or(&model),
        fallback.as_deref(),
    );
    response
}

async fn dispatch(
    backend: std::sync::Arc<dyn BackendProvider>,
    request: InteractionRequest,
    raw_failure: &Mutex<Option<Response>>,
) -> Result<reqwest::Response, BackendError> {
    *raw_failure.lock().unwrap() = None;
    let upstream = backend
        .send_gemini_interaction(&request.headers, request.body(), request.streaming())
        .await?;
    let status = upstream.status();
    if status.as_u16() != 429 && !status.is_server_error() {
        return Ok(upstream);
    }
    let headers = forwardable_upstream_headers(upstream.headers());
    let builder = response_builder(&upstream);
    let bytes = upstream.bytes().await?;
    let mut response = builder.body(Body::from(bytes)).unwrap();
    super::stamp_trace_headers(response.headers_mut());
    *raw_failure.lock().unwrap() = Some(response);
    Err(BackendError::api_error(
        "Gemini upstream request failed",
        status.as_u16(),
        headers,
    ))
}

fn response_builder(upstream: &reqwest::Response) -> axum::http::response::Builder {
    let mut builder = Response::builder().status(upstream.status()).header(
        header::CONTENT_TYPE,
        upstream
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/json"),
    );
    for (name, value) in forwardable_upstream_headers(upstream.headers()) {
        builder = builder.header(name, value);
    }
    builder
}

async fn forward(
    upstream: reqwest::Response,
    state: &crate::AppState,
    api_key: &ResolvedApiKey,
    resolution: &RouteResolution,
    span: LlmSpanWriter,
    permit: Option<QuotaPermit>,
) -> Response {
    let builder = response_builder(&upstream);
    if !upstream.status().is_success() {
        if let Some(permit) = permit {
            permit.finish_failed().await;
        }
        return builder
            .body(Body::from_stream(upstream.bytes_stream()))
            .unwrap();
    }
    let accounting = Accounting {
        state: state.clone(),
        attribution: UsageAttribution::new(api_key, resolution),
        span,
        permit,
        usage: None,
        finalized: false,
    };
    if upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.contains("text/event-stream"))
    {
        return builder
            .body(Body::from_stream(InteractionTap::new(
                upstream.bytes_stream(),
                accounting,
            )))
            .unwrap();
    }
    let mut accounting = accounting;
    let bytes = match upstream.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => return super::backend_error_response(&error.into()),
    };
    if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
        accounting.observe(&value);
        stamp_content(&accounting.span, ContentDirection::Output, &value);
        accounting
            .span
            .apply_outcome_sampling(&state.sampling_gate, &redacted(&value));
    }
    if accounting.finish(true).await.is_err() {
        return super::usage_write_error_response();
    }
    builder.body(Body::from(bytes)).unwrap()
}

/// Signatures are returned to the caller; only the telemetry copy is redacted.
fn redacted(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(redacted).collect()),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        if matches!(
                            key.as_str(),
                            "signature" | "thought_signature" | "thoughtSignature"
                        ) {
                            Value::String("[redacted]".into())
                        } else {
                            redacted(value)
                        },
                    )
                })
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn stamp_content(span: &LlmSpanWriter, direction: ContentDirection, value: &Value) {
    if !span.content_enabled() {
        return;
    }
    let value = redacted(value);
    span.record_json(direction, &value);
    let (prefix, role, content) = match direction {
        ContentDirection::Input => ("llm.input_messages.0.message", "user", value.get("input")),
        ContentDirection::Output => (
            "llm.output_messages.0.message",
            "assistant",
            value.get("steps").or_else(|| value.get("events")),
        ),
    };
    if let Some(content) = content {
        span.try_metadata(direction, format!("{prefix}.role"), role);
        span.try_content(
            direction,
            format!("{prefix}.content"),
            content
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| content.to_string()),
        );
    }
}

fn token_usage(value: &Value) -> Option<TokenUsage> {
    let input = value.get("total_input_tokens")?.as_u64()?;
    let output = value.get("total_output_tokens")?.as_u64()?;
    let optional = |name| match value.get(name) {
        Some(value) => value.as_u64(),
        None => Some(0),
    };
    let cached = optional("total_cached_tokens")?;
    let thoughts = optional("total_thought_tokens")?;
    let tools = optional("total_tool_use_tokens")?;
    if cached > input {
        return None;
    }
    // Gemini lists thought tokens and tool-use prompt tokens separately.
    let input = input.checked_add(tools)?;
    let output = output.checked_add(thoughts)?;
    Some(TokenUsage {
        input_tokens: input,
        output_tokens: output,
        output_tokens_observed: true,
        cached_input_tokens: cached,
        reasoning_tokens: thoughts,
        total_tokens: input.checked_add(output)?,
        usage_source: crate::usage_tracker::UsageSource::Reported,
        ..TokenUsage::default()
    })
}

struct Accounting {
    state: crate::AppState,
    attribution: UsageAttribution,
    span: LlmSpanWriter,
    permit: Option<QuotaPermit>,
    usage: Option<TokenUsage>,
    finalized: bool,
}
impl Accounting {
    fn observe(&mut self, value: &Value) {
        if let Some(usage) = value.get("usage").and_then(token_usage) {
            self.usage = Some(usage);
        }
        if let Some(status) = value.get("status").and_then(Value::as_str) {
            self.span.set_finish_reason(status);
        }
    }
    fn finish(&mut self, complete: bool) -> UsageWriteFuture {
        if self.finalized {
            return usage::empty_usage_future();
        }
        self.finalized = true;
        let write = match self.usage {
            Some(usage) => {
                usage.stamp(&self.span);
                usage::record_future(self.state.clone(), self.attribution.clone(), usage)
            }
            None if complete => usage::missing_usage_future(),
            None => usage::empty_usage_future(),
        };
        usage::with_quota_settlement(
            write,
            self.permit.take(),
            if complete {
                self.usage.map(|usage| usage.total_tokens)
            } else {
                None
            },
        )
    }
}
impl Drop for Accounting {
    fn drop(&mut self) {
        if !self.finalized {
            usage::detach_future(self.finish(false));
        }
    }
}

/// Observe complete frames for usage while forwarding the original bytes,
/// including event IDs, comments, unknown event types, and signed content.
struct InteractionTap {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    decoder: SseDecoder,
    wire: BytesMut,
    accounting: Accounting,
    terminal: bool,
    stopped: bool,
    trace_events: Vec<Value>,
    trace_remaining: usize,
    trace_finished: bool,
    pending: Option<UsageWriteFuture>,
    held: Option<Bytes>,
}
impl InteractionTap {
    fn new(
        inner: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
        accounting: Accounting,
    ) -> Self {
        let trace_remaining = if accounting.span.content_enabled() {
            accounting
                .span
                .content_bytes_remaining(ContentDirection::Output)
                .saturating_sub(32)
        } else {
            0
        };
        Self {
            inner: Box::pin(inner),
            trace_events: Vec::new(),
            trace_remaining,
            trace_finished: false,
            decoder: SseDecoder::default(),
            wire: BytesMut::new(),
            accounting,
            terminal: false,
            stopped: false,
            pending: None,
            held: None,
        }
    }
    fn finish_trace(&mut self) {
        if self.trace_finished {
            return;
        }
        self.trace_finished = true;
        if !self.trace_events.is_empty() {
            stamp_content(
                &self.accounting.span,
                ContentDirection::Output,
                &json!({"events": std::mem::take(&mut self.trace_events)}),
            );
        }
    }

    fn observe(&mut self, bytes: &[u8]) {
        for frame in self.decoder.push(bytes) {
            let Ok(value) = serde_json::from_str::<Value>(&frame.data) else {
                continue;
            };
            if self.trace_remaining > 0 {
                let event = redacted(&value);
                let size = event.to_string().len().saturating_add(1);
                if size <= self.trace_remaining {
                    self.trace_events.push(event);
                    self.trace_remaining -= size;
                } else {
                    self.trace_remaining = 0;
                    self.accounting.span.try_metadata(
                        ContentDirection::Output,
                        "gemini.output.truncated",
                        "true",
                    );
                }
            }
            if let Some(interaction) = value.get("interaction") {
                self.accounting.observe(interaction);
            }
            self.accounting.observe(&value);
            let kind = value["event_type"]
                .as_str()
                .or(frame.event.as_deref())
                .unwrap_or_default();
            if !self.terminal
                && matches!(
                    kind,
                    "interaction.completed"
                        | "interaction.failed"
                        | "interaction.cancelled"
                        | "error"
                )
            {
                self.terminal = true;
                let complete = kind == "interaction.completed";
                if !complete {
                    self.accounting
                        .span
                        .set_error("gemini_interaction_failed", None);
                }
                self.finish_trace();
                if let Some(interaction) = value.get("interaction") {
                    self.accounting.span.apply_outcome_sampling(
                        &self.accounting.state.sampling_gate,
                        &redacted(interaction),
                    );
                }
                self.pending = Some(self.accounting.finish(complete));
            }
        }
    }
}

fn stream_error(code: &str, message: &str) -> Bytes {
    let (_, error) = ErrorResponse::new(StatusCode::BAD_GATEWAY, code, message);
    let value = json!({"event_type":"error", "error":{"code":error.error.error_type, "message":error.error.message}});
    encode_frame(Some("error"), &value.to_string())
}

impl Stream for InteractionTap {
    type Item = Result<Bytes, BackendError>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let span = this.accounting.span.clone();
        let _enter = span.enter();
        loop {
            if let Some(write) = this.pending.as_mut() {
                match write.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(result) => {
                        this.pending = None;
                        if let Err(error) = result {
                            this.held = None;
                            this.stopped = true;
                            span.set_error(
                                "usage_accounting_unavailable",
                                Some(&error.to_string()),
                            );
                            return Poll::Ready(Some(Ok(stream_error(
                                "usage_accounting_unavailable",
                                "Usage accounting is temporarily unavailable",
                            ))));
                        }
                        if let Some(bytes) = this.held.take() {
                            return Poll::Ready(Some(Ok(bytes)));
                        }
                    }
                }
            }
            if this.stopped {
                return Poll::Ready(None);
            }
            match this.inner.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(bytes))) => {
                    this.wire.extend_from_slice(&bytes);
                    this.observe(&bytes);
                    let complete = this.wire.len() - this.decoder.pending_bytes();
                    if complete > 0 {
                        let bytes = this.wire.split_to(complete).freeze();
                        if this.pending.is_some() {
                            this.held = Some(bytes);
                        } else {
                            return Poll::Ready(Some(Ok(bytes)));
                        }
                    }
                }
                Poll::Ready(Some(Err(error))) => {
                    this.stopped = true;
                    span.set_error("stream_transport_error", Some(&error.to_string()));
                    usage::detach_future(this.accounting.finish(false));
                    return Poll::Ready(Some(Err(error.into())));
                }
                Poll::Ready(None) => {
                    this.stopped = true;
                    if this.terminal && !this.wire.is_empty() {
                        return Poll::Ready(Some(Ok(this.wire.split().freeze())));
                    }
                    if !this.terminal {
                        span.set_error("incomplete_stream", None);
                        this.held = Some(stream_error(
                            "incomplete_stream",
                            "Gemini stream ended before a terminal interaction event",
                        ));
                        this.pending = Some(this.accounting.finish(false));
                    }
                }
            }
        }
    }
}
impl Drop for InteractionTap {
    fn drop(&mut self) {
        self.finish_trace();
        if let Some(write) = self.pending.take() {
            usage::detach_future(write);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_request_rewrites_only_model_without_rounding_or_reordering_content() {
        let raw = br#"{"model":"google/models/gemini-2.5-flash","store":false,"input":[ {"type":"thought","signature":"sig=="}, {"type":"function_result","result":{"z":184467440737095516160,"a":1.2300e-4}} ],"future":{"z":2,"a":1}}"#;
        let mut request = InteractionRequest::parse(raw, HeaderMap::new()).unwrap();
        let before: BTreeMap<String, Box<RawValue>> = serde_json::from_slice(raw).unwrap();
        request.set_model("models/gemini-2.5-flash".into());
        let after: BTreeMap<String, Box<RawValue>> =
            serde_json::from_slice(&request.body()).unwrap();
        for (key, value) in before {
            if key != "model" {
                assert_eq!(after[&key].get(), value.get());
            }
        }
        assert_eq!(after["model"].get(), "\"gemini-2.5-flash\"");
        assert!(!request.may_fallback());
        assert!(request.conservative_input_tokens().is_some());
    }

    #[test]
    fn native_usage_counts_thinking_and_tool_prompts_once() {
        let value = json!({"total_input_tokens":10,"total_output_tokens":2,"total_thought_tokens":3,"total_tool_use_tokens":1,"total_cached_tokens":4,"total_tokens":16});
        let usage = token_usage(&value).unwrap();
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.total_tokens),
            (11, 5, 16)
        );
        assert_eq!(usage.reasoning_tokens, 3);
        assert_eq!(usage.cached_input_tokens, 4);
        for patch in [
            json!({"total_input_tokens":-1}),
            json!({"total_output_tokens":null}),
            json!({"total_cached_tokens":100}),
            json!({"total_thought_tokens":u64::MAX}),
        ] {
            let mut invalid = value.clone();
            invalid
                .as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            assert!(token_usage(&invalid).is_none());
        }
        assert_eq!(
            token_usage(&json!({"total_input_tokens":0,"total_output_tokens":0}))
                .unwrap()
                .total_tokens,
            0
        );
    }

    #[test]
    fn only_telemetry_copy_redacts_signatures() {
        let value = json!({"steps":[{"type":"thought","signature":"private"},{"type":"model_output","content":[{"type":"text","text":"answer","thoughtSignature":"also-private"}]}]});
        let copy = redacted(&value).to_string();
        assert!(!copy.contains("private"));
        assert!(copy.contains("answer"));
        assert_eq!(value["steps"][0]["signature"], "private");
    }
}
