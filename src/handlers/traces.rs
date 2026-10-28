//! Client span ingestion.
//!
//! `POST /v1/traces/spans` lets a caller (e.g. the opencode tracing plugin)
//! submit already-finished spans — session, turn, reasoning, and tool spans —
//! that chalk-router emits through its own OpenTelemetry pipeline. Combined
//! with `traceparent` propagation on the completion endpoints (see
//! `telemetry::OtelMakeSpan`), this lets a client stitch the router's LLM
//! spans and its own agent-side spans into a single OpenInference-style trace
//! with an optional root session span.
//!
//! Clients choose their own trace/span ids (W3C hex) and timestamps, so spans
//! can be reported after the fact without any server-side session state.

use axum::{
    extract::Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use opentelemetry::{
    global,
    trace::{
        Span as _, SpanContext, SpanId, SpanKind, Status, TraceContextExt as _, TraceFlags,
        TraceId, TraceState, Tracer as _, TracerProvider as _,
    },
    Context, KeyValue,
};
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::middleware::ResolvedApiKey;
use crate::types::{Error, ErrorResponse};

/// Upper bound on spans per request, mirroring typical OTLP batch limits.
const MAX_SPANS_PER_REQUEST: usize = 1024;
/// Keep client-authored spans within the same per-span payload envelope used by
/// router-generated LLM spans. This makes the shared processor's 16-span export
/// cap meaningful and rejects oversized input instead of silently overflowing
/// the collector or truncating attributes in the SDK.
const MAX_INGESTED_SPAN_BYTES: usize =
    crate::handlers::trace_attributes::MAX_LLM_SPAN_CONTENT_BYTES;
// Slots the router adds to every ingested span: session, verified environment
// and sampling decision (stamped by processors), plus the span source and the
// verified caller's principal ID, principal type and user ID.
const PROCESSOR_ATTRIBUTE_RESERVE: usize = 7;
/// Spans may end at most this far ahead of the router's clock (client skew).
const MAX_SPAN_CLOCK_SKEW: Duration = Duration::from_secs(5 * 60);
/// Oldest accepted span start, for spans reported after the fact.
const MAX_SPAN_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Longest accepted span, long enough for a full-day agent session.
const MAX_SPAN_DURATION: Duration = Duration::from_secs(24 * 60 * 60);

/// The OpenInference span kind recorded for each ingest kind. Clients can
/// override by passing an explicit `openinference.span.kind` attribute.
fn openinference_kind(kind: IngestSpanKind) -> &'static str {
    match kind {
        IngestSpanKind::Session | IngestSpanKind::Agent => "AGENT",
        IngestSpanKind::Turn | IngestSpanKind::Reasoning | IngestSpanKind::Chain => "CHAIN",
        IngestSpanKind::Tool => "TOOL",
        IngestSpanKind::Llm => "LLM",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestSpanKind {
    Session,
    Turn,
    Reasoning,
    Tool,
    Agent,
    Chain,
    Llm,
}

/// Nanoseconds since the unix epoch. Accepts a JSON number or a decimal
/// string (JS callers should send strings: ns values exceed 2^53).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum UnixNanos {
    Num(u64),
    Str(String),
}

impl UnixNanos {
    fn to_system_time(&self) -> Result<SystemTime, String> {
        let nanos: u64 = match self {
            UnixNanos::Num(n) => *n,
            UnixNanos::Str(s) => s
                .trim()
                .parse()
                .map_err(|_| format!("invalid unix-nano timestamp: {s:?}"))?,
        };
        Ok(UNIX_EPOCH + Duration::from_nanos(nanos))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestStatusCode {
    Ok,
    Error,
    Unset,
}

#[derive(Debug, Deserialize)]
pub struct IngestStatus {
    pub code: IngestStatusCode,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct IngestSpan {
    /// Display name, e.g. "session", "turn 3", "tool: bash".
    pub name: String,
    pub kind: IngestSpanKind,
    /// 32-char lowercase hex W3C trace id (non-zero).
    pub trace_id: String,
    /// 16-char lowercase hex W3C span id (non-zero).
    pub span_id: String,
    /// Optional 16-char hex parent span id within the same trace.
    #[serde(default)]
    pub parent_span_id: Option<String>,
    pub start_time_unix_nano: UnixNanos,
    pub end_time_unix_nano: UnixNanos,
    /// Attribute map. Strings, bools, and numbers are recorded natively;
    /// arrays and objects are recorded as JSON strings.
    #[serde(default)]
    pub attributes: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub status: Option<IngestStatus>,
}

#[derive(Debug, Deserialize)]
pub struct IngestSpansRequest {
    pub spans: Vec<IngestSpan>,
}

#[derive(Debug, Serialize)]
pub struct IngestSpansResponse {
    pub accepted: usize,
}

fn bad_request(message: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse {
            error: Error {
                message,
                error_type: "invalid_request_error".to_string(),
                param: None,
                code: None,
            },
        }),
    )
        .into_response()
}

fn validate_span_count(count: usize) -> Result<(), String> {
    (count <= MAX_SPANS_PER_REQUEST)
        .then_some(())
        .ok_or_else(|| format!("too many spans: {count} (max {MAX_SPANS_PER_REQUEST})"))
}

fn json_to_otel_value(v: &serde_json::Value) -> opentelemetry::Value {
    match v {
        serde_json::Value::String(s) => opentelemetry::Value::String(s.clone().into()),
        serde_json::Value::Bool(b) => opentelemetry::Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                opentelemetry::Value::I64(i)
            } else {
                opentelemetry::Value::F64(n.as_f64().unwrap_or(0.0))
            }
        }
        other => opentelemetry::Value::String(other.to_string().into()),
    }
}

/// Validate one ingest span and convert it into everything needed to emit it.
#[derive(Debug)]
struct ValidatedSpan {
    name: String,
    trace_id: TraceId,
    span_id: SpanId,
    parent_span_id: Option<SpanId>,
    start: SystemTime,
    end: SystemTime,
    attributes: Vec<KeyValue>,
    status: Status,
}

fn is_reserved_client_attribute(key: &str) -> bool {
    key.starts_with("chalk.") || key == "user.id"
}

fn validate_span(
    span: &IngestSpan,
    index: usize,
    now: SystemTime,
) -> Result<ValidatedSpan, String> {
    let err = |msg: String| format!("spans[{index}]: {msg}");

    if span.name.is_empty() {
        return Err(err("name must be non-empty".to_string()));
    }

    // `from_hex` is a bare radix parse, so enforce the W3C fixed widths here.
    let parse_trace_id = |s: &str| -> Option<TraceId> {
        if s.len() != 32 {
            return None;
        }
        TraceId::from_hex(s).ok()
    };
    let parse_span_id = |s: &str| -> Option<SpanId> {
        if s.len() != 16 {
            return None;
        }
        SpanId::from_hex(s).ok()
    };

    let trace_id = parse_trace_id(&span.trace_id).ok_or_else(|| {
        err(format!(
            "invalid trace_id {:?} (expected 32 hex chars)",
            span.trace_id
        ))
    })?;
    if trace_id == TraceId::INVALID {
        return Err(err("trace_id must be non-zero".to_string()));
    }
    let span_id = parse_span_id(&span.span_id).ok_or_else(|| {
        err(format!(
            "invalid span_id {:?} (expected 16 hex chars)",
            span.span_id
        ))
    })?;
    if span_id == SpanId::INVALID {
        return Err(err("span_id must be non-zero".to_string()));
    }
    let parent_span_id = match &span.parent_span_id {
        Some(p) => {
            let id = parse_span_id(p).ok_or_else(|| {
                err(format!(
                    "invalid parent_span_id {p:?} (expected 16 hex chars)"
                ))
            })?;
            if id == SpanId::INVALID {
                return Err(err("parent_span_id must be non-zero".to_string()));
            }
            if id == span_id {
                return Err(err("parent_span_id must differ from span_id".to_string()));
            }
            Some(id)
        }
        None => None,
    };

    let start = span.start_time_unix_nano.to_system_time().map_err(&err)?;
    let end = span.end_time_unix_nano.to_system_time().map_err(&err)?;
    if end < start {
        return Err(err(
            "end_time_unix_nano is before start_time_unix_nano".to_string()
        ));
    }
    // Bound timestamps so implausible clocks cannot land spans far outside
    // the trace store's time partitions or the sampling history window.
    if end > now + MAX_SPAN_CLOCK_SKEW {
        return Err(err(format!(
            "end_time_unix_nano is more than {}s in the future",
            MAX_SPAN_CLOCK_SKEW.as_secs()
        )));
    }
    if now
        .checked_sub(MAX_SPAN_AGE)
        .is_some_and(|oldest| start < oldest)
    {
        return Err(err(format!(
            "start_time_unix_nano is more than {}s old",
            MAX_SPAN_AGE.as_secs()
        )));
    }
    if end.duration_since(start).unwrap_or_default() > MAX_SPAN_DURATION {
        return Err(err(format!(
            "span lasts more than {}s",
            MAX_SPAN_DURATION.as_secs()
        )));
    }

    let implicit_kind = !span.attributes.contains_key("openinference.span.kind");
    let attribute_count = span.attributes.len() + usize::from(implicit_kind);
    let max_client_attributes =
        crate::telemetry::span_attribute_count_limit().saturating_sub(PROCESSOR_ATTRIBUTE_RESERVE);
    if attribute_count > max_client_attributes {
        return Err(err(format!(
            "too many attributes: {attribute_count} (max {max_client_attributes})"
        )));
    }
    let attribute_bytes = serde_json::to_vec(&span.attributes)
        .map_err(|error| err(format!("attributes are not serializable: {error}")))?
        .len();
    let status_message_bytes = span
        .status
        .as_ref()
        .and_then(|status| status.message.as_ref())
        .map_or(0, String::len);
    let span_bytes = attribute_bytes
        .saturating_add(span.name.len())
        .saturating_add(status_message_bytes);
    if span_bytes > MAX_INGESTED_SPAN_BYTES {
        return Err(err(format!(
            "span is too large: {span_bytes} bytes (max {MAX_INGESTED_SPAN_BYTES})"
        )));
    }

    let mut attributes: Vec<KeyValue> = Vec::with_capacity(attribute_count);
    if implicit_kind {
        attributes.push(KeyValue::new(
            "openinference.span.kind",
            openinference_kind(span.kind),
        ));
    }
    for (k, v) in &span.attributes {
        attributes.push(KeyValue::new(k.clone(), json_to_otel_value(v)));
    }

    let status = match &span.status {
        Some(IngestStatus {
            code: IngestStatusCode::Ok,
            ..
        }) => Status::Ok,
        Some(IngestStatus {
            code: IngestStatusCode::Error,
            message,
        }) => Status::Error {
            description: message.clone().unwrap_or_default().into(),
        },
        Some(IngestStatus {
            code: IngestStatusCode::Unset,
            ..
        })
        | None => Status::Unset,
    };

    Ok(ValidatedSpan {
        name: span.name.clone(),
        trace_id,
        span_id,
        parent_span_id,
        start,
        end,
        attributes,
        status,
    })
}

/// Emit a validated span through the global tracer provider so it flows out
/// the same OTLP pipeline as the router's own spans.
fn emit_span(
    mut v: ValidatedSpan,
    state: &crate::AppState,
    match_context: &crate::sampling_policy::MatchContext,
    stamped: &[KeyValue],
) {
    let tracer = global::tracer_provider().tracer("chalk-router-ingest");

    let session = v
        .attributes
        .iter()
        .find(|kv| kv.key.as_str() == crate::telemetry::SESSION_ID_ATTRIBUTE)
        .map(|kv| kv.value.as_str().to_string());
    let key = session.clone().unwrap_or_else(|| v.trace_id.to_string());
    let decision = state
        .sampling_gate
        .decide(&key, match_context, session.is_some());
    crate::handlers::trace_attributes::LlmSpanWriter::apply_client_content_policy(
        &mut v.attributes,
        decision.log_content,
    );
    // After the content policy, so disabling content never drops these.
    v.attributes.extend_from_slice(stamped);

    let mut builder = tracer
        .span_builder(v.name)
        .with_kind(SpanKind::Internal)
        .with_span_id(v.span_id)
        .with_start_time(v.start)
        .with_attributes(v.attributes)
        .with_status(v.status);

    let parent_cx = match v.parent_span_id {
        Some(parent_id) => {
            // A remote-parent context carries the trace id, so the builder
            // must not also set one.
            Context::new().with_remote_span_context(SpanContext::new(
                v.trace_id,
                parent_id,
                TraceFlags::SAMPLED,
                true,
                TraceState::default(),
            ))
        }
        None => {
            builder = builder.with_trace_id(v.trace_id);
            Context::new()
        }
    };

    let parent_cx =
        crate::telemetry::sampling::with_sampling_inputs(parent_cx, None, false, Some(decision))
            .with_value(crate::telemetry::sampling::TenantSampling {
                environment_id: state.environment_id.clone(),
                gate: state.sampling_gate.clone(),
            });

    let mut span = tracer.build_with_context(builder, &parent_cx);
    span.end_with_timestamp(v.end);
}

pub async fn ingest_spans(
    crate::tenancy::RequestState(state): crate::tenancy::RequestState<crate::AppState>,
    api_key: ResolvedApiKey,
    Json(mut req): Json<IngestSpansRequest>,
) -> Response {
    // This is an API contract, not an exporter-throughput setting. In
    // particular it must not shrink when a pod chooses a smaller BSP queue.
    if let Err(message) = validate_span_count(req.spans.len()) {
        return bad_request(message);
    }

    // `chalk.*` attributes are the router's own routing, sampling and identity
    // controls, and `user.id` names the verified caller. Drop client-supplied
    // copies instead of rejecting the batch: exporters trust these keys, so the
    // router stamps its own values below and never forwards a client's.
    let mut dropped = 0usize;
    for span in &mut req.spans {
        let before = span.attributes.len();
        span.attributes
            .retain(|key, _| !is_reserved_client_attribute(key));
        dropped += before - span.attributes.len();
    }
    if dropped > 0 {
        tracing::debug!(dropped, "Dropped reserved attributes from client spans");
    }

    let now = SystemTime::now();
    let mut validated = Vec::with_capacity(req.spans.len());
    for (i, span) in req.spans.iter().enumerate() {
        match validate_span(span, i, now) {
            Ok(v) => validated.push(v),
            Err(msg) => return bad_request(msg),
        }
    }

    let accepted = validated.len();
    let match_context = crate::sampling_policy::MatchContext {
        key_id: Some(api_key.0.key_prefix.clone()),
        usage_pool_id: api_key.0.usage_pool_id.clone(),
        request_tag: None,
    };
    let stamped = crate::handlers::trace_attributes::client_span_router_attributes(
        api_key.authenticated_principal_id(),
        api_key.authenticated_principal_type(),
        api_key.authenticated_user_id(),
    );
    for v in validated {
        emit_span(v, &state, &match_context, &stamped);
    }

    tracing::debug!(accepted, "Ingested client spans");
    (StatusCode::OK, Json(IngestSpansResponse { accepted })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shortly after `base_span`, which starts one second after the epoch.
    fn test_now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(60)
    }

    fn base_span() -> IngestSpan {
        IngestSpan {
            name: "session".to_string(),
            kind: IngestSpanKind::Session,
            trace_id: "0af7651916cd43dd8448eb211c80319c".to_string(),
            span_id: "b7ad6b7169203331".to_string(),
            parent_span_id: None,
            start_time_unix_nano: UnixNanos::Num(1_000_000_000),
            end_time_unix_nano: UnixNanos::Num(2_000_000_000),
            attributes: serde_json::Map::new(),
            status: None,
        }
    }

    #[test]
    fn validate_accepts_well_formed_span() {
        let v = validate_span(&base_span(), 0, test_now()).expect("should validate");
        assert_eq!(v.trace_id.to_string(), "0af7651916cd43dd8448eb211c80319c");
        assert_eq!(v.span_id.to_string(), "b7ad6b7169203331");
        assert!(v.parent_span_id.is_none());
        // The default OpenInference kind is derived from the ingest kind.
        assert!(v.attributes.iter().any(|kv| {
            kv.key.as_str() == "openinference.span.kind" && kv.value.as_str() == "AGENT"
        }));
    }

    fn span_between(start: SystemTime, end: SystemTime) -> IngestSpan {
        let nanos = |at: SystemTime| {
            UnixNanos::Num(at.duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64)
        };
        IngestSpan {
            start_time_unix_nano: nanos(start),
            end_time_unix_nano: nanos(end),
            ..base_span()
        }
    }

    #[test]
    fn validate_bounds_timestamps_to_a_plausible_window() {
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let second = Duration::from_secs(1);
        let accepts =
            |start: SystemTime, end: SystemTime| validate_span(&span_between(start, end), 0, now);

        // Ends up to the allowed skew ahead of the router's clock.
        assert!(accepts(now, now + MAX_SPAN_CLOCK_SKEW).is_ok());
        assert!(accepts(now, now + MAX_SPAN_CLOCK_SKEW + second)
            .unwrap_err()
            .contains("in the future"));

        // Starts up to the maximum age in the past.
        let oldest = now - MAX_SPAN_AGE;
        assert!(accepts(oldest, oldest + second).is_ok());
        assert!(accepts(oldest - second, oldest)
            .unwrap_err()
            .contains("old"));

        // Lasts up to the maximum duration.
        let start = now - MAX_SPAN_DURATION - second;
        assert!(accepts(start, start + MAX_SPAN_DURATION).is_ok());
        assert!(accepts(start, start + MAX_SPAN_DURATION + second)
            .unwrap_err()
            .contains("lasts more than"));
    }

    #[test]
    fn request_limit_remains_the_documented_1024_spans() {
        assert!(validate_span_count(1_024).is_ok());
        assert_eq!(
            validate_span_count(1_025).unwrap_err(),
            "too many spans: 1025 (max 1024)"
        );
    }

    #[test]
    fn validate_rejects_bad_ids_and_times() {
        let mut s = base_span();
        s.trace_id = "not-hex".to_string();
        assert!(validate_span(&s, 0, test_now())
            .unwrap_err()
            .contains("trace_id"));

        let mut s = base_span();
        s.trace_id = "0".repeat(32);
        assert!(validate_span(&s, 0, test_now())
            .unwrap_err()
            .contains("non-zero"));

        let mut s = base_span();
        s.span_id = "abc".to_string();
        assert!(validate_span(&s, 0, test_now())
            .unwrap_err()
            .contains("span_id"));

        let mut s = base_span();
        s.parent_span_id = Some(s.span_id.clone());
        assert!(validate_span(&s, 0, test_now())
            .unwrap_err()
            .contains("differ"));

        let mut s = base_span();
        s.end_time_unix_nano = UnixNanos::Num(0);
        assert!(validate_span(&s, 0, test_now())
            .unwrap_err()
            .contains("before"));
    }

    #[test]
    fn validate_parses_string_nanos() {
        let mut s = base_span();
        s.start_time_unix_nano = UnixNanos::Str("1750000000000000000".to_string());
        s.end_time_unix_nano = UnixNanos::Str("1750000001000000000".to_string());
        let now = UNIX_EPOCH + Duration::from_secs(1_750_000_060);
        let v = validate_span(&s, 0, now).expect("string nanos should parse");
        assert_eq!(
            v.end.duration_since(v.start).unwrap(),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn validate_respects_explicit_openinference_kind() {
        let mut s = base_span();
        s.attributes.insert(
            "openinference.span.kind".to_string(),
            serde_json::json!("EVALUATOR"),
        );
        let v = validate_span(&s, 0, test_now()).unwrap();
        let kinds: Vec<_> = v
            .attributes
            .iter()
            .filter(|kv| kv.key.as_str() == "openinference.span.kind")
            .collect();
        assert_eq!(kinds.len(), 1);
        assert_eq!(kinds[0].value.as_str(), "EVALUATOR");
    }

    #[test]
    fn validate_converts_complex_attributes_to_json_strings() {
        let mut s = base_span();
        s.attributes
            .insert("count".to_string(), serde_json::json!(3));
        s.attributes
            .insert("ratio".to_string(), serde_json::json!(0.5));
        s.attributes
            .insert("flag".to_string(), serde_json::json!(true));
        s.attributes
            .insert("nested".to_string(), serde_json::json!({"a": 1}));
        let v = validate_span(&s, 0, test_now()).unwrap();
        let get = |key: &str| {
            v.attributes
                .iter()
                .find(|kv| kv.key.as_str() == key)
                .map(|kv| kv.value.clone())
                .unwrap()
        };
        assert_eq!(get("count"), opentelemetry::Value::I64(3));
        assert_eq!(get("ratio"), opentelemetry::Value::F64(0.5));
        assert_eq!(get("flag"), opentelemetry::Value::Bool(true));
        assert_eq!(get("nested").as_str(), r#"{"a":1}"#);
    }

    #[test]
    fn validate_rejects_attributes_that_cannot_fit_a_safe_export_batch() {
        let mut span = base_span();
        span.attributes.insert(
            "input.value".to_string(),
            serde_json::json!("x".repeat(MAX_INGESTED_SPAN_BYTES)),
        );

        assert!(validate_span(&span, 0, test_now())
            .unwrap_err()
            .contains("too large"));
    }

    #[test]
    fn validate_rejects_attributes_the_sdk_would_silently_truncate() {
        let mut span = base_span();
        for index in 0..crate::telemetry::span_attribute_count_limit() {
            span.attributes
                .insert(format!("attribute.{index}"), serde_json::json!(index));
        }

        assert!(validate_span(&span, 0, test_now())
            .unwrap_err()
            .contains("too many attributes"));
    }

    #[test]
    fn validate_reserves_the_session_processor_attribute_slot() {
        let mut span = base_span();
        span.attributes.insert(
            "openinference.span.kind".to_string(),
            serde_json::json!("AGENT"),
        );
        for index in 0..crate::telemetry::span_attribute_count_limit() - PROCESSOR_ATTRIBUTE_RESERVE
        {
            span.attributes
                .insert(format!("attribute.{index}"), serde_json::json!(index));
        }

        assert!(validate_span(&span, 0, test_now())
            .unwrap_err()
            .contains("too many attributes"));
    }

    #[test]
    fn validate_bounds_name_and_status_message_with_attributes() {
        let mut named = base_span();
        named.name = "n".repeat(MAX_INGESTED_SPAN_BYTES + 1);
        assert!(validate_span(&named, 0, test_now())
            .unwrap_err()
            .contains("too large"));

        let mut status = base_span();
        status.status = Some(IngestStatus {
            code: IngestStatusCode::Error,
            message: Some("e".repeat(MAX_INGESTED_SPAN_BYTES + 1)),
        });
        assert!(validate_span(&status, 0, test_now())
            .unwrap_err()
            .contains("too large"));
    }

    #[test]
    fn kind_mapping_matches_openinference() {
        assert_eq!(openinference_kind(IngestSpanKind::Session), "AGENT");
        assert_eq!(openinference_kind(IngestSpanKind::Turn), "CHAIN");
        assert_eq!(openinference_kind(IngestSpanKind::Reasoning), "CHAIN");
        assert_eq!(openinference_kind(IngestSpanKind::Tool), "TOOL");
        assert_eq!(openinference_kind(IngestSpanKind::Agent), "AGENT");
        assert_eq!(openinference_kind(IngestSpanKind::Chain), "CHAIN");
        assert_eq!(openinference_kind(IngestSpanKind::Llm), "LLM");
    }
}
