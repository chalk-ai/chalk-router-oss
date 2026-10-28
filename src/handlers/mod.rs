//! # Generation surfaces
//! Authenticated requests select an environment runtime through `tenancy::RequestState`;
//! every existing pipeline stage continues to consume that same runtime.

//!
//! Every request-serving surface below resolves a route, may retry onto
//! route-owned fallbacks, checks capability support, acquires quota/rate-limit,
//! dispatches to a backend, and settles usage against the permit it acquired.
//! Surfaces differ in which stages apply and how; this table is the map that
//! `routing::run_passthrough`'s doc comment points back to.
//! Foreground requests commit folded request quota and route admission separately
//! to shorten shared-policy lock lifetimes, then settle permits atomically with
//! durable usage through the shared helpers.
//! Proxy surfaces use the shared auth middleware, with an explicit opt-in for
//! anonymous local development; HTTP and standard gRPC health probes are exempt.
//!
//! | Surface                     | Resolve + fallback                                    | Capability gate                                      | Quota / rate-limit                              | Usage settlement                | Judge | Prompt cache | Debug capture |
//! |------------------------------|--------------------------------------------------------|-------------------------------------------------------|---------------------------------------------------|-----------------------------------|-------|---------------|-----------------|
//! | `files` upload              | disabled (501), no body extraction or storage access | no file grants exist; model grants never authorize upload | no admission until file quotas/lifecycle exist | none | no | no | no |
//! | `chat` create                | [`routing::try_with_fallbacks_if`]                     | [`routing::RoutedRequest::check_capability`]        | folded request quota (opaque parts: soft cap with reserve) + per-attempt rate limit | [`usage::with_quota_settlement`]  | yes   | yes           | yes             |
//! | `alpha/decisions` / `decisions` (OpenAI) / `systemone` create | [`routing::try_with_fallbacks_if`] | ingress protocol (OpenAI translated to Router Decisions) + Decisions model capability + image input / box feature | folded request quota + per-attempt rate limit | [`usage::with_quota_settlement`] | no | no | no |
//! | `responses` create           | [`routing::try_with_fallbacks_if`]                     | request-shape + adapter translation checks + response-locator match | folded request quota + per-attempt rate limit | [`usage::with_quota_settlement`] | self-contained requests | no | no |
//! | `responses` foreground create | [`routing::try_with_fallbacks_if`]                    | request-shape predicate + response-locator match       | folded quota (hosted tools, stored history, opaque content: soft cap with reserve) + per-attempt rate limit | [`usage::with_quota_settlement`]  | no    | no            | no              |
//! | `responses` background create | [`routing::try_with_fallbacks_if`]                    | explicit native/adapted Background + retrieval support | durable folded request quota + per-attempt rate limit | durable background reconciler     | no    | no            | no              |
//! | `responses` retrieve/delete/cancel | response locator pins one connection             | native/adapted operation profile                       | request-only single-route quota                    | observe tracked background terminal state | no | no         | no              |
//! | `responses` compact          | single route (opaque compaction item has no cross-provider equivalent) | native/adapted `DeliveryMode` check                    | [`routing::acquire_single_route_quota`] (hosted tools, stored history, opaque content: soft cap with reserve) | [`usage::with_quota_settlement`]  | no    | no            | no              |
//! | `messages` create             | [`routing::try_with_fallbacks_if`]                     | [`routing::RoutedRequest::check_capability`]        | folded request quota (server tools, opaque content: soft cap with reserve) + per-attempt rate limit | [`usage::with_quota_settlement`]  | no    | yes           | no              |
//! | `gemini` interactions create | [`routing::try_with_fallbacks_if`] (only fresh stateless requests may fall back) | native Gemini operation + request requirements | folded request quota + per-attempt rate limit | [`usage::with_quota_settlement`] | no | no (opaque history and Google storage semantics) | no |
//! | `messages` count_tokens      | single route (token counts are provider-specific, not fallback-eligible) | native dispatch only                                   | [`routing::acquire_single_route_quota`]            | none (no billable usage)          | no    | no            | no              |
//! | `embeddings`                 | single route (`state.backend.resolve`)                 | [`routing::check_passthrough_capabilities`]            | [`routing::acquire_single_route_quota`] (Gemini image document: 8,192-token ceiling) | [`usage::with_quota_settlement`] | no | no       | no              |
//! | `images` generate/edit       | single route (`state.backend.resolve`)                 | [`routing::check_passthrough_capabilities`] (request, then per-image) | [`routing::acquire_single_route_quota`] | direct `permit.finish`, no token usage | no | no       | no              |
//! | `moderations` text create     | [`routing::run_passthrough`]                          | folded into `run_passthrough`                          | [`routing::acquire_single_route_quota`] (request-only) | [`usage::with_quota_settlement`] (zero tokens) | no | no | no |
//! | `audio` speech/transcription/translation | [`routing::run_passthrough`] (includes permission) | folded into `run_passthrough`                          | [`routing::acquire_request_quota`]                 | direct `permit.finish`, no token usage | no | no       | no              |
//!
//! HTTP model deletion returns 501 without reading request state, stores, or providers.
//! Route deletion belongs exclusively to the authorized gRPC admin surface.
//!
//! Foreground fallback walks keep separate request and route grants. Each phase
//! fails open independently on backend errors without replaying an ambiguous
//! admission; route denials retain the request RPM charge. An exhausted pool
//! skips the route phase instead of waiting on the pool again, and a quota row
//! lock wait past the admission `lock_timeout` fails closed with a 429. Later
//! fallback candidates keep per-attempt admission; background ownership keeps
//! its durable acquisition path.
//!
//! Schema-bearing requests retain declaration order at dispatch, including native
//! Responses compaction and Jev chat translation. This changes serialization,
//! not the stage applicability shown above.
//! Chat and translated Responses check adapter compatibility before dispatch;
//! classified Claude reasoning rejections may use fallbacks before a response opens.
//!
//! Native Responses JSON and SSE retain upstream success status, content type,
//! and body bytes. Only response locator ID tokens are rewritten; observations
//! for tracing, foreground settlement, and background reconciliation use a
//! separate parsed view. Native SSE must have a text/event-stream content type
//! before dispatch succeeds. These envelope rules do not bypass any stage above.
//!
//! Conversation judging runs once before the attempt list is assembled, using
//! `crate::judge::input::JudgeInput` and `crate::judge::routing`. Responses with
//! external history or stored prompts skip judging and retain existing connection
//! affinity and fallback eligibility checks. All fallback
//! lists contain concrete connection/model destinations and are never expanded
//! recursively. Response caching is bypassed for routes with routing policies
//! and for Chat requests with fallback overrides.
//! Chat request fallback keys and alternatives accept exact connection names
//! as well as IDs; they are normalized to IDs before judging and dispatch.
//!
//! A surface with no fallback in the table never retries onto an alternate
//! model: compaction because its opaque item has no equivalent elsewhere,
//! count_tokens because token counts are provider-specific, and the
//! single-route surfaces because `run_passthrough`/`check_passthrough_capabilities`
//! only ever resolve the one model the caller named.
//! Reported text/image token attribution prices Gemini embeddings consistently
//! in traces and durable usage; a missing or invalid split leaves cost unpriced
//! while preserving authoritative total tokens for settlement.
//!
//! Gemini image embeddings use the same embedding stages, with native API
//! translation in the backend. Gemini Embedding 2 image documents reserve the
//! model's maximum 8,192 input tokens, then settle to reported usage; this needs
//! 8,192 available token-budget/TPM capacity even for a smaller image. Other
//! providers retain unknown image reservations. Combined text/image parts are one
//! document. Image tracing fetches follow permission, capability, and quota admission;
//! reported usage is required independently of the admission bound. Image downloads
//! always use public-only egress, independently of provider connection trust.
//!
//! Responses and Gemini stored-history references have unknown token
//! reservations. The shared quota stage rejects them when a matching token
//! budget or TPM policy applies; request-rate and concurrency policies still
//! admit them normally. Inline history remains eligible for text estimation.
//! Responses provider-hosted tools also have unknown token reservations and
//! require authoritative usage for settlement; caller-run function/custom tools remain
//! eligible for text estimation. This applies to foreground and background.
//!
//! Generation rate limits match both bare and prefixed IDs on the connection
//! that owns its provider's canonical prefix. Existing selectors share their
//! policy counters across spellings; custom prefixes and chalk aliases stay exact.
//!
//! Model discovery (`models` list, VS Code projection, and single-model lookup)
//! authenticates through middleware and checks restricted keys against resolved
//! routes. Listings filter after shared catalog reads; lookup checks before
//! fetching details. Discovery does not generate content or consume generation
//! quota, and has no capability gate, fallback, or usage settlement.
//!
//! Audio currently buffers multipart input and native response bodies. Its
//! response envelope preserves formats and metadata; incremental audio SSE
//! delivery needs streaming quota ownership and is not provided by this path.
//!
//! Gemini agent interactions return 501 after authentication and shape
//! validation, before model resolution: these requests name an agent rather
//! than a model and do not enter the generation pipeline.

pub mod anthropic;
pub mod assistants;
pub mod audio;
pub mod batch;
pub mod chat;
pub mod decisions;
pub mod embeddings;
pub mod files;
pub mod fine_tuning;
pub mod gemini;
pub mod images;
pub mod models;
pub mod moderations;
pub mod responses;
pub(crate) mod routing;
pub(crate) mod sse;
pub(crate) mod trace_attributes;
pub mod traces;
pub(crate) mod usage;

use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use opentelemetry::trace::TraceContextExt;
use std::sync::OnceLock;
use std::time::Duration;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::backend::BackendError;
use crate::middleware::ResolvedApiKey;
use crate::prompt_cache::{CacheContext, CachedCompletion, PayloadKind};
use crate::types::{Error, ErrorResponse};

/// Cached at startup: whether request/response *content* (prompts, messages,
/// embedding input text, model outputs) is recorded on spans. Metadata (model,
/// system, token counts, dimensions, latency, ...) is always recorded.
static LOG_CONTENT: OnceLock<bool> = OnceLock::new();

/// Initialize the process-wide privacy gate from validated RouterConfig.
/// Without initialization, content capture stays off.
pub fn init_content_logging(enabled: bool) {
    let enabled = *LOG_CONTENT.get_or_init(|| enabled);
    tracing::info!(
        "Trace content logging: {}",
        if enabled {
            "enabled (full)"
        } else {
            "metadata-only"
        }
    );
}

/// Whether to record request/response content on spans, for the request being
/// served.
///
/// Two gates, ANDed. `ROUTER_TRACE_LOG_CONTENT` is the process-wide kill switch
/// (initialized at boot, off if initialization is skipped). The sampling policy
/// further restricts each request. An unreadable policy forbids capture; routes
/// without a sampling decision still require the process-wide opt-in.
///
/// The order matters: an operator who has switched content logging off for the
/// deployment must not have it turned back on by a policy edit.
pub fn content_logging_enabled() -> bool {
    *LOG_CONTENT.get().unwrap_or(&false) && policy_allows_content()
}

/// The matched sampling rule's `log_content`, read from the request's baggage.
///
/// Baggage rather than a request extension: this is resolved where the rule is
/// matched, in `telemetry::OtelMakeSpan`, which receives the request by
/// reference and cannot insert an extension. It is the same channel the rest of
/// the request-scoped sampling facts already travel on, and it reaches handler
/// code through the span it is writing.
fn policy_allows_content() -> bool {
    crate::telemetry::sampling::bool_from_baggage(
        &tracing::Span::current().context(),
        crate::telemetry::SAMPLING_LOG_CONTENT_ATTRIBUTE,
    )
    .unwrap_or(true)
}

/// Stamp the server-verified caller identity through the bounded LLM span
/// writer. Request-authenticated JWTs and stored router API keys supply it;
/// caller-provided protocol fields never do.
pub(crate) fn stamp_authenticated_principal(
    span: &trace_attributes::LlmSpanWriter,
    api_key: &ResolvedApiKey,
) {
    span.set_principal(
        api_key.authenticated_principal_id(),
        api_key.authenticated_principal_type(),
    );
    span.set_user_id(api_key.authenticated_user_id());
}

/// Read the OpenTelemetry trace_id and span_id from the currently active
/// `tracing` span, formatted as lowercase hex.
///
/// Note: if no OTel span context is active (or it is invalid/non-sampled), the
/// ids come back as all-zeros (`00000000000000000000000000000000` /
/// `0000000000000000`). We still return them so callers can attach them.
pub fn current_trace_ids() -> (String, String) {
    let ctx = tracing::Span::current().context();
    let span = ctx.span();
    let sc = span.span_context();
    (sc.trace_id().to_string(), sc.span_id().to_string())
}

/// Map a [`BackendError`] to the status code, forwardable headers, and OpenAI
/// `error.type` we surface to the caller.
///
/// `ApiError` preserves the upstream HTTP status (so e.g. a provider 429 reaches
/// the caller as a 429, not a 500) and carries rate-limit / retry headers worth
/// forwarding. Unreadable responses map to 502, local routing deadlines to 504,
/// and other errors to 500.
fn backend_error_parts(e: &BackendError) -> (StatusCode, &[(String, String)], &'static str) {
    match e {
        BackendError::InvalidResponse(_) => (StatusCode::BAD_GATEWAY, &[], "backend_error"),
        BackendError::RoutingDeadlineExceeded => {
            (StatusCode::GATEWAY_TIMEOUT, &[], "backend_error")
        }
        BackendError::PermissionDenied(_) => (StatusCode::FORBIDDEN, &[], "permission_error"),
        BackendError::ApiError {
            status, headers, ..
        } => {
            let code = StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (code, headers.as_slice(), synthesized_error_kind(code).0)
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, &[], "backend_error"),
    }
}

/// OpenAI-style `error.type` and `error.code` for an error body the router
/// writes itself (a router-originated error, or an upstream error whose
/// original body is not retained). 4xx statuses take the `type`/`code` pairs
/// OpenAI uses, so SDKs classify them the way they classify OpenAI's own
/// errors; 5xx and transport failures stay `backend_error`. A 401 is also
/// left as `backend_error`: the router's own key checks answer from the auth
/// middleware with `invalid_api_key`, so a 401 here is the upstream rejecting
/// the router's credential, and labelling it `authentication_error` would
/// send the caller to rotate a key that is not the problem.
fn synthesized_error_kind(status: StatusCode) -> (&'static str, Option<&'static str>) {
    match status {
        StatusCode::UNAUTHORIZED => ("backend_error", None),
        // The router's own denials are `PermissionDenied`, which carries
        // `access_denied`; a 403 here is an upstream refusing the router's
        // credential (an IAM policy, a model not enabled), so it keeps the
        // type without claiming the caller's key was denied.
        StatusCode::FORBIDDEN => ("permission_error", None),
        StatusCode::TOO_MANY_REQUESTS => ("rate_limit_error", Some("rate_limit_exceeded")),
        status if status.is_client_error() => ("invalid_request_error", None),
        _ => ("backend_error", None),
    }
}

/// The OpenAI-shaped error body for a backend error, used both for JSON error
/// responses and for the terminal error frame of a stream that fails after
/// the 200 is committed. An `ApiError` keeps its upstream or router message
/// verbatim, without the `API error (N):` prefix of its `Display` form.
pub fn backend_error_body(e: &BackendError) -> ErrorResponse {
    let (status, _, error_type) = backend_error_parts(e);
    let (message, param, code) = match e {
        BackendError::ApiError { message, .. } => {
            let (param, code) = if e.is_model_not_found() {
                (
                    Some("model".to_string()),
                    Some("model_not_found".to_string()),
                )
            } else {
                (None, synthesized_error_kind(status).1.map(str::to_string))
            };
            (message.clone(), param, code)
        }
        BackendError::PermissionDenied(message) => {
            (message.clone(), None, Some("access_denied".to_string()))
        }
        other => (other.to_string(), None, None),
    };
    ErrorResponse {
        error: Error {
            message,
            error_type: error_type.to_string(),
            param,
            code,
        },
    }
}

/// Build a client-facing JSON error response from a backend error, preserving
/// the upstream status and forwarding rate-limit / retry headers. Always stamps
/// trace headers. Used both for non-streaming responses and for streaming
/// requests that fail *before* the stream opens (no 200 committed yet).
pub fn backend_error_response(e: &BackendError) -> Response {
    backend_error_response_for_format(e, crate::backend::provider::UpstreamErrorFormat::Openai)
}

pub fn backend_error_response_for_format(
    e: &BackendError,
    format: crate::backend::provider::UpstreamErrorFormat,
) -> Response {
    let (status, headers, _) = backend_error_parts(e);
    if let BackendError::ApiError {
        original: Some(original),
        ..
    } = e
    {
        if original.format == format {
            let mut response = Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, &original.content_type)
                .body(axum::body::Body::from(original.body.clone()))
                .expect("valid retained upstream response");
            apply_forwarded_headers(response.headers_mut(), headers);
            stamp_trace_headers(response.headers_mut());
            return response;
        }
    }
    let mut res = (status, Json(backend_error_body(e))).into_response();
    apply_forwarded_headers(res.headers_mut(), headers);
    stamp_trace_headers(res.headers_mut());
    res
}

pub fn native_body_response(upstream: crate::backend::provider::BackendBodyResponse) -> Response {
    let mut response = Response::builder()
        .status(upstream.status)
        .header(header::CONTENT_TYPE, upstream.content_type)
        .body(axum::body::Body::from(upstream.body))
        .expect("native HTTP response");
    apply_forwarded_headers(response.headers_mut(), &upstream.headers);
    stamp_trace_headers(response.headers_mut());
    response
}

/// Build a 403 permission-denied response in the OpenAI error shape, matching
/// the `permission_error` / `access_denied` code the auth middleware uses. Used
/// when a key's provider/model allowlist rejects the resolved route. Stamps
/// trace headers like the other handler responses.
pub fn forbidden_response(message: impl Into<String>) -> Response {
    let body = ErrorResponse {
        error: Error {
            message: message.into(),
            error_type: "permission_error".to_string(),
            param: None,
            code: Some("access_denied".to_string()),
        },
    };
    let mut res = (StatusCode::FORBIDDEN, Json(body)).into_response();
    stamp_trace_headers(res.headers_mut());
    res
}

/// Build a protocol-neutral request-validation error. Attribution headers are
/// parsed before a frontend-specific body is dispatched, so the OpenAI error
/// envelope is used consistently across proxy surfaces.
pub fn invalid_request_response(message: impl Into<String>) -> Response {
    let body = ErrorResponse {
        error: Error {
            message: message.into(),
            error_type: "invalid_request_error".to_string(),
            param: None,
            code: Some("invalid_request".to_string()),
        },
    };
    let mut res = (StatusCode::BAD_REQUEST, Json(body)).into_response();
    stamp_trace_headers(res.headers_mut());
    res
}

/// Build a 404 response for a path that does not belong to any router API.
///
/// This deliberately uses the same OpenAI-style JSON envelope as validation
/// errors so a client cannot mistake an unmatched path for a successful gRPC
/// response.
pub fn not_found_response() -> Response {
    let mut res = ErrorResponse::new(
        StatusCode::NOT_FOUND,
        "invalid_request_error",
        "Unknown router path",
    )
    .into_response();
    stamp_trace_headers(res.headers_mut());
    res
}

/// Do not acknowledge an upstream success when its usage event could not be
/// durably accepted. The database error is logged by the ledger and is never
/// exposed to the caller.
pub fn usage_write_error_response() -> Response {
    let body = ErrorResponse {
        error: Error {
            message: "Usage accounting is temporarily unavailable".to_string(),
            error_type: "server_error".to_string(),
            param: None,
            code: Some("usage_accounting_unavailable".to_string()),
        },
    };
    let mut res = (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response();
    stamp_trace_headers(res.headers_mut());
    res
}

/// Build a 501 response for a route that is mounted (for OpenAI API-surface
/// compatibility) but has no backing implementation. Used instead of
/// fabricating a fake success body, so callers can tell "not supported" apart
/// from a real result. `feature` names the capability in the error message,
/// e.g. "Batch jobs" or "Image editing".
pub fn not_implemented(feature: impl Into<String>) -> Response {
    let body = ErrorResponse {
        error: Error {
            message: format!("{} not yet supported", feature.into()),
            error_type: "not_implemented".to_string(),
            param: None,
            code: None,
        },
    };
    let mut res = (StatusCode::NOT_IMPLEMENTED, Json(body)).into_response();
    stamp_trace_headers(res.headers_mut());
    res
}

/// Insert forwarded upstream headers (rate-limit / retry signaling) onto a
/// response. Invalid header names/values are silently skipped.
pub fn apply_forwarded_headers(
    headers: &mut axum::http::HeaderMap,
    forwarded: &[(String, String)],
) {
    for (name, value) in forwarded {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(n, v);
        }
    }
}

/// Attach `x-trace-id` / `x-span-id` headers to a response's headers from the
/// currently active span. Invalid header values are silently skipped.
pub fn stamp_trace_headers(headers: &mut axum::http::HeaderMap) {
    let (trace_id, span_id) = current_trace_ids();
    if let Ok(val) = HeaderValue::from_str(&trace_id) {
        headers.insert("x-trace-id", val);
    }
    if let Ok(val) = HeaderValue::from_str(&span_id) {
        headers.insert("x-span-id", val);
    }
}

// ---------------------------------------------------------------------------
// Exact-prefix response cache: pieces shared by the surfaces that use it
// ---------------------------------------------------------------------------
//
// `/v1/chat/completions` (chat.rs) and the `/v1/messages` passthrough
// (anthropic.rs) cache different payloads — a canonical completion vs. the
// upstream bytes verbatim — but agree on how a cache decision is reported and
// how an entry is written. See `crate::prompt_cache`.

/// Header telling the caller whether the router answered this request from its
/// own cache. Only ever present when the key has caching enabled, so its
/// absence means "not cached", not "missed".
const CACHE_STATUS_HEADER: &str = "x-chalk-cache";
/// The hit's hash address, so a hit can be tied back to the miss that stored it.
const CACHE_KEY_HEADER: &str = "x-chalk-cache-key";
/// How long ago (whole seconds) the served entry was stored.
const CACHE_AGE_HEADER: &str = "x-chalk-cache-age";

/// Record the cache decision on the span and the response headers.
///
/// `hit` carries the served entry's age; `None` is a miss. Both are stamped —
/// a miss is as diagnostic as a hit when someone is asking why their hit rate
/// is zero.
pub(crate) fn stamp_cache_outcome(
    span: &trace_attributes::LlmSpanWriter,
    headers: Option<&mut header::HeaderMap>,
    backend: &'static str,
    ctx: &CacheContext,
    hit: Option<Duration>,
) {
    let status = if hit.is_some() { "hit" } else { "miss" };
    let key_hex = ctx.key.to_hex();
    span.set_cache_outcome(status, backend, ctx.scope.as_str(), &key_hex, hit);
    let Some(headers) = headers else { return };
    headers.insert(
        HeaderName::from_static(CACHE_STATUS_HEADER),
        HeaderValue::from_static(status),
    );
    // The digest is hex, so both header values are always valid header bytes.
    if let Ok(value) = HeaderValue::from_str(&key_hex) {
        headers.insert(HeaderName::from_static(CACHE_KEY_HEADER), value);
    }
    if let Some(age) = hit {
        if let Ok(value) = HeaderValue::from_str(&age.as_secs().to_string()) {
            headers.insert(HeaderName::from_static(CACHE_AGE_HEADER), value);
        }
    }
}

/// Store a completed response, off the request path.
///
/// Spawned rather than awaited: a `put` against a shared cache is a network
/// round-trip, and making the client wait for it would hand back part of the
/// latency the cache exists to remove. A dropped write just means the next
/// identical request misses.
pub(crate) fn store_completion(
    state: &crate::AppState,
    ctx: &CacheContext,
    payload: Vec<u8>,
    payload_kind: PayloadKind,
    served_model: String,
) {
    let value = CachedCompletion::new(payload, payload_kind, served_model);
    if !ctx.admits(&value) {
        tracing::debug!(
            bytes = value.weight(),
            max_entry_bytes = ctx.max_entry_bytes,
            "prompt cache: response exceeds the entry ceiling; not cached"
        );
        return;
    }
    let cache = state.prompt_cache.clone();
    let key = ctx.key;
    let ttl = ctx.ttl;
    tokio::spawn(async move {
        cache.put(&key, value, ttl).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_error_response_preserves_429_and_forwards_rate_limit_headers() {
        let e = BackendError::api_error(
            "slow down",
            429,
            vec![
                ("retry-after".to_string(), "30".to_string()),
                (
                    "anthropic-ratelimit-requests-remaining".to_string(),
                    "0".to_string(),
                ),
            ],
        );
        let res = backend_error_response(&e);
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(res.headers().get("retry-after").unwrap(), "30");
        assert_eq!(
            res.headers()
                .get("anthropic-ratelimit-requests-remaining")
                .unwrap(),
            "0"
        );
    }

    #[test]
    fn router_originated_errors_take_openai_error_types_and_codes() {
        let body = backend_error_body(&BackendError::model_not_found("chalk/nope"));
        assert_eq!(
            body.error.message,
            "model route 'chalk/nope' does not exist"
        );
        assert_eq!(body.error.error_type, "invalid_request_error");
        assert_eq!(body.error.param.as_deref(), Some("model"));
        assert_eq!(body.error.code.as_deref(), Some("model_not_found"));

        let body = backend_error_body(&BackendError::api_status(400, "n must be 1"));
        assert_eq!(
            body.error.message, "n must be 1",
            "no `API error (N):` prefix"
        );
        assert_eq!(body.error.error_type, "invalid_request_error");
        assert_eq!(body.error.code, None);

        let body = backend_error_body(&BackendError::PermissionDenied("model denied".into()));
        assert_eq!(body.error.error_type, "permission_error");
        assert_eq!(body.error.code.as_deref(), Some("access_denied"));

        // An upstream 403 keeps the type but not the router's own code.
        let body = backend_error_body(&BackendError::api_status(403, "AccessDeniedException"));
        assert_eq!(body.error.error_type, "permission_error");
        assert_eq!(body.error.code, None);

        let body = backend_error_body(&BackendError::api_status(429, "slow down"));
        assert_eq!(body.error.error_type, "rate_limit_error");
        assert_eq!(body.error.code.as_deref(), Some("rate_limit_exceeded"));

        let body = backend_error_body(&BackendError::api_status(502, "upstream broke"));
        assert_eq!(body.error.error_type, "backend_error");
        assert_eq!(body.error.code, None);

        // An upstream 401 is the router's credential, not the caller's key.
        let body = backend_error_body(&BackendError::api_status(401, "bad upstream key"));
        assert_eq!(body.error.error_type, "backend_error");
        assert_eq!(body.error.code, None);

        // An upstream 404 that is not the router's own unknown-model error
        // does not claim `model_not_found`.
        let body = backend_error_body(&BackendError::api_status(404, "no such response"));
        assert_eq!(body.error.error_type, "invalid_request_error");
        assert_eq!(body.error.code, None);
        assert_eq!(body.error.param, None);
    }

    #[test]
    fn backend_error_response_maps_non_api_errors_to_500() {
        let res = backend_error_response(&BackendError::Other("boom".to_string()));
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn backend_error_response_preserves_other_upstream_statuses() {
        let e = BackendError::api_status(502, "bad gateway");
        let res = backend_error_response(&e);
        assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    }
}
