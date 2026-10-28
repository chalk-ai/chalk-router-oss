//! The sampling gate drops a trace whole, and decides a conversation once.
//!
//! Four things this pins down, all of which are properties of the *wiring*
//! rather than of the hash:
//!
//! * The gate governs only the LLM frontends. An admin route exports at the
//!   configured rate 0.0 exactly as it did before the policy existed.
//! * Two requests of one conversation — different traces, same `session.id` —
//!   get the same answer, which is the whole reason the key is the session and
//!   not the trace.
//! * The rate inputs a caller supplies — the request tag, the force-trace
//!   header — reach the handler's own LLM span and not only the request span,
//!   so a thinned trace is thinned whole rather than losing its root and
//!   keeping its body.
//! * A dropped trace takes its *untagged* spans with it. A request opens spans
//!   that carry no LLM markers at all, and they are the ones a marker-based
//!   filter would leak.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::post,
    Router,
};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::export::trace::SpanData;
use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
use opentelemetry_sdk::trace::{SimpleSpanProcessor, TracerProvider};
use tower::ServiceExt;
use tower_http::trace::TraceLayer;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::layer::SubscriberExt;

use openai_server::api_keys::{ApiKey, ApiKeyPermission};
use openai_server::sampling_policy::{
    InMemorySamplingPolicyStore, SamplingPolicy, SamplingPolicyStore, SamplingRule,
};
use openai_server::telemetry::sampling::{
    SamplingContextSpanProcessor, SamplingGate, SamplingGateSpanProcessor,
};
use openai_server::telemetry::{
    text_map_propagator, OtelMakeSpan, OtelOnResponse, SessionIdSpanProcessor,
    CALLER_REQUEST_TAG_ATTRIBUTE, FORCE_TRACE_HEADER, REQUEST_TAG_HEADER,
    SAMPLING_DECISION_ATTRIBUTE, SAMPLING_FORCE_KEEP_ATTRIBUTE, SAMPLING_LOG_CONTENT_ATTRIBUTE,
    SAMPLING_OUTCOME_RATE_ATTRIBUTE,
};

/// Stands in for a chat-completion handler: the request span's child, carrying
/// the OpenInference kind that makes it policy-gated in its own right.
async fn llm_handler(
    axum::Extension(gate): axum::Extension<Arc<SamplingGate>>,
    headers: axum::http::HeaderMap,
) -> StatusCode {
    let span = tracing::info_span!(
        "chat_completion",
        otel.name = "chat.completion",
        openinference.span.kind = "LLM",
    );
    let entered = span.enter();
    if headers.contains_key("x-test-saturate-outcomes") {
        span.set_attribute(SAMPLING_OUTCOME_RATE_ATTRIBUTE, 1.0);
    }
    drop(entered);
    drop(span); // resolve the LLM outcome before closing the enclosing HTTP span
    if headers.contains_key("x-test-saturate-outcomes") {
        for i in 1..=8_300u128 {
            gate.resolve_outcome(opentelemetry::trace::TraceId::from(i), "evict", 0.0, false);
        }
    }
    StatusCode::OK
}

/// An LLM handler with a child span carrying none of the LLM markers — the
/// shape of the `policy_cache_refresh` the fallback and judge policy stores open
/// when their cached policy has gone stale while a request is being served.
/// Nothing about that span says which trace it belongs to except the trace
/// itself.
async fn llm_handler_with_untagged_child() -> StatusCode {
    let span = tracing::info_span!(
        "anthropic_messages",
        otel.name = "anthropic.messages",
        openinference.span.kind = "LLM",
    );
    let _entered = span.enter();
    // Drop order closes this before its parent, child-first as in production.
    let untagged = tracing::info_span!("db_budget_check");
    let _untagged = untagged.enter();
    StatusCode::OK
}

/// Reports what content logging resolved to for this request, so a test can
/// assert on the boolean the real `LlmSpanWriter` gates its content setters on.
async fn llm_handler_reporting_content_logging() -> StatusCode {
    let span = tracing::info_span!(
        "create_embedding",
        otel.name = "embedding.create",
        openinference.span.kind = "EMBEDDING",
    );
    let _entered = span.enter();
    tracing::Span::current().set_attribute(
        CONTENT_LOGGING_PROBE,
        openai_server::handlers::content_logging_enabled(),
    );
    StatusCode::OK
}

/// Stands in for a handler that evaluated a response-content outcome rule
/// (`LlmSpanWriter::apply_outcome_sampling`) and, optionally, force-kept the
/// trace on top of it (`LlmSpanWriter::set_sampling_force_keep`) — both read
/// from test-only headers so one handler covers every precedence scenario.
async fn llm_handler_with_outcome(headers: axum::http::HeaderMap) -> StatusCode {
    let span = tracing::info_span!(
        "chat_completion",
        otel.name = "chat.completion",
        openinference.span.kind = "LLM",
    );
    let _entered = span.enter();
    if let Some(rate) = headers
        .get("x-test-outcome-rate")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<f64>().ok())
    {
        tracing::Span::current().set_attribute(SAMPLING_OUTCOME_RATE_ATTRIBUTE, rate);
    }
    if headers.contains_key("x-test-force-keep") {
        tracing::Span::current().set_attribute(SAMPLING_FORCE_KEEP_ATTRIBUTE, true);
    }
    StatusCode::OK
}

/// Fails the way an upstream outage does: an error status, after the handler has
/// already opened and closed its LLM span.
async fn llm_handler_failing() -> StatusCode {
    let span = tracing::info_span!(
        "moderation",
        otel.name = "moderation.create",
        openinference.span.kind = "LLM",
    );
    let _entered = span.enter();
    StatusCode::BAD_GATEWAY
}

async fn admin_handler() -> StatusCode {
    StatusCode::OK
}

/// Test-only attribute carrying `content_logging_enabled()`'s answer.
const CONTENT_LOGGING_PROBE: &str = "test.content_logging";

/// The router's layer order: auth resolves the caller, then the span is made.
///
/// The layer and the gate processor share one `SamplingGate`, as `main` wires
/// them: the request path decides the trace and the export path both honors that
/// decision and holds the force-keep set.
fn app(key: Arc<ApiKey>, gate: Arc<SamplingGate>) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(llm_handler))
        .route("/v1/messages", post(llm_handler_with_untagged_child))
        .route(
            "/v1/embeddings",
            post(llm_handler_reporting_content_logging),
        )
        .route("/v1/moderations", post(llm_handler_failing))
        .route("/v1/outcome", post(llm_handler_with_outcome))
        .route("/v1/admin/keys", post(admin_handler))
        .layer(axum::Extension(gate.clone()))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(OtelMakeSpan::new(gate.clone()))
                // The same hook `main` installs: an errored response keeps its
                // trace whatever rate matched.
                .on_response(OtelOnResponse::new(gate)),
        )
        .layer(axum::middleware::from_fn(
            move |mut req: Request<Body>, next: axum::middleware::Next| {
                let key = key.clone();
                async move {
                    req.extensions_mut().insert(key);
                    next.run(req).await
                }
            },
        ))
}

/// The caller every request below authenticates as. Its prefix is what a
/// `key_id` rule matches, and its pool what a `usage_pool_id` rule matches.
const TEST_KEY_ID: &str = "ck-testkey1";

fn key_in_pool(usage_pool_id: Option<&str>) -> Arc<ApiKey> {
    Arc::new(ApiKey {
        key_hash: "hash".to_string(),
        key_prefix: TEST_KEY_ID.to_string(),
        description: String::new(),
        created_at: chrono::Utc::now(),
        permission: ApiKeyPermission::default(),
        labels: Default::default(),
        usage_pool_id: usage_pool_id.map(str::to_string),
        daily_token_budget: None,
    })
}

struct RequestSpec {
    path: &'static str,
    headers: Vec<(&'static str, String)>,
    usage_pool_id: Option<&'static str>,
    expected_status: StatusCode,
}

impl RequestSpec {
    fn at(path: &'static str) -> Self {
        Self {
            path,
            headers: Vec::new(),
            usage_pool_id: None,
            expected_status: StatusCode::OK,
        }
    }

    fn llm() -> Self {
        Self::at("/v1/chat/completions")
    }

    /// An LLM route whose handler also opens an untagged span.
    fn llm_with_untagged_child() -> Self {
        Self::at("/v1/messages")
    }

    /// An LLM route that reports what content logging resolved to.
    fn llm_reporting_content_logging() -> Self {
        Self::at("/v1/embeddings")
    }

    /// An LLM route whose response is an error.
    fn llm_failing() -> Self {
        let mut spec = Self::at("/v1/moderations");
        spec.expected_status = StatusCode::BAD_GATEWAY;
        spec
    }

    /// An LLM route whose handler stamps an outcome rate (and, optionally
    /// with `.header("x-test-force-keep", "1")`, a force-keep) instead of
    /// depending on real response content.
    fn llm_outcome() -> Self {
        Self::at("/v1/outcome")
    }

    fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    fn in_pool(mut self, usage_pool_id: &'static str) -> Self {
        self.usage_pool_id = Some(usage_pool_id);
        self
    }
}

/// A one-condition rule, which is the least a valid rule states.
fn rule(field: &str, value: &str, rate: f64) -> SamplingRule {
    let mut rule = SamplingRule {
        request_tag: None,
        key_id: None,
        usage_pool_id: None,
        rate,
        log_content: true,
    };
    match field {
        "request_tag" => rule.request_tag = Some(value.to_string()),
        "key_id" => rule.key_id = Some(value.to_string()),
        "usage_pool_id" => rule.usage_pool_id = Some(value.to_string()),
        other => panic!("unknown condition {other}"),
    }
    rule
}

/// What the two gated destinations each received.
///
/// `main` wraps the OTLP batch processor and the Braintrust processor in
/// separate `SamplingGateSpanProcessor`s that share one gate; these two stand in
/// for that pair, so a test can check they agree.
struct ExportedSpans {
    primary: Vec<SpanData>,
    secondary: Vec<SpanData>,
}

impl ExportedSpans {
    fn names(&self) -> Vec<String> {
        self.primary
            .iter()
            .map(|span| span.name.to_string())
            .collect()
    }

    fn span(&self, name: &str) -> Option<&SpanData> {
        self.primary.iter().find(|span| span.name == name)
    }

    /// Every attribute key the primary destination received, across all spans.
    fn attribute_keys(&self) -> Vec<String> {
        self.primary
            .iter()
            .flat_map(|span| span.attributes.iter())
            .map(|kv| kv.key.as_str().to_string())
            .collect()
    }
}

fn bool_attribute(span: &SpanData, key: &str) -> Option<bool> {
    span.attributes.iter().find_map(|kv| match kv.value {
        opentelemetry::Value::Bool(value) if kv.key.as_str() == key => Some(value),
        _ => None,
    })
}

/// Drive requests through the real layer stack and the real gate, returning the
/// spans that made it past the gate to each destination.
async fn exported_spans(policy: SamplingPolicy, requests: Vec<RequestSpec>) -> ExportedSpans {
    let gate = Arc::new(SamplingGate::new());
    let store: Arc<dyn SamplingPolicyStore> = Arc::new(InMemorySamplingPolicyStore::new(policy));
    gate.set_store(store);
    // Prime it, as `main` does at boot: an unread policy fails open, which
    // would make every assertion below vacuous.
    gate.refresh().await;

    let primary = InMemorySpanExporter::default();
    let secondary = InMemorySpanExporter::default();
    let provider = TracerProvider::builder()
        .with_span_processor(SessionIdSpanProcessor)
        .with_span_processor(SamplingContextSpanProcessor)
        .with_span_processor(SamplingGateSpanProcessor::new(
            SimpleSpanProcessor::new(Box::new(primary.clone())),
            gate.clone(),
        ))
        .with_span_processor(SamplingGateSpanProcessor::new(
            SimpleSpanProcessor::new(Box::new(secondary.clone())),
            gate.clone(),
        ))
        .build();
    opentelemetry::global::set_text_map_propagator(text_map_propagator());

    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));

    {
        // `set_default` is thread-local, so the requests have to be driven with
        // `oneshot` rather than a spawned server.
        let _guard = tracing::subscriber::set_default(subscriber);
        for spec in requests {
            let mut request = Request::builder()
                .method("POST")
                .uri(spec.path)
                .header("content-type", "application/json");
            for (name, value) in &spec.headers {
                request = request.header(*name, value);
            }
            let response = app(key_in_pool(spec.usage_pool_id), gate.clone())
                .oneshot(request.body(Body::from("{}")).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), spec.expected_status);
        }
    }

    ExportedSpans {
        primary: primary.get_finished_spans().unwrap(),
        secondary: secondary.get_finished_spans().unwrap(),
    }
}

/// The names of the spans the primary destination received.
async fn exported_span_names(policy: SamplingPolicy, requests: Vec<RequestSpec>) -> Vec<String> {
    exported_spans(policy, requests).await.names()
}

/// The request span exports under its `otel.name`, not the macro's name.
const REQUEST_SPAN: &str = "POST /v1/chat/completions";
const MESSAGES_REQUEST_SPAN: &str = "POST /v1/messages";
const MODERATIONS_REQUEST_SPAN: &str = "POST /v1/moderations";
const ADMIN_SPAN: &str = "POST /v1/admin/keys";
const OUTCOME_REQUEST_SPAN: &str = "POST /v1/outcome";

fn drop_everything() -> SamplingPolicy {
    SamplingPolicy {
        enabled: true,
        default_rate: 0.0,
        ..SamplingPolicy::default()
    }
}

#[tokio::test]
async fn a_zero_rate_drops_frontend_spans_and_leaves_every_other_route_alone() {
    let exported = exported_span_names(
        drop_everything(),
        vec![RequestSpec::llm(), RequestSpec::at("/v1/admin/keys")],
    )
    .await;

    assert!(
        !exported.iter().any(|name| name == "chat.completion"),
        "the LLM span must be gated: {exported:?}"
    );
    assert!(
        !exported.iter().any(|name| name == REQUEST_SPAN),
        "and so must the frontend request span: {exported:?}"
    );
    assert!(
        exported.iter().any(|name| name == ADMIN_SPAN),
        "a non-frontend route is untouched by the policy: {exported:?}"
    );
}

#[tokio::test]
async fn a_dropped_trace_takes_its_untagged_spans_with_it() {
    // `db_budget_check` carries no `chalk.frontend*` and no
    // `openinference.span.kind`, so nothing about the span itself says it
    // belongs to an LLM request. Its trace's decision is what drops it.
    let dropped = exported_span_names(
        drop_everything(),
        vec![RequestSpec::llm_with_untagged_child()],
    )
    .await;

    assert!(
        dropped.is_empty(),
        "a dropped frontend trace must export nothing at all, \
         including the spans with no LLM markers: {dropped:?}"
    );

    // And the span really is created and exportable — otherwise the assertion
    // above would hold just as well for a handler that never opened it.
    let kept = exported_span_names(
        SamplingPolicy::default(),
        vec![RequestSpec::llm_with_untagged_child()],
    )
    .await;

    for expected in [
        "db_budget_check",
        "anthropic.messages",
        MESSAGES_REQUEST_SPAN,
    ] {
        assert!(
            kept.iter().any(|name| name == expected),
            "{expected} should export while the policy is off: {kept:?}"
        );
    }
}

#[tokio::test]
async fn a_disabled_policy_keeps_everything() {
    let exported = exported_span_names(SamplingPolicy::default(), vec![RequestSpec::llm()]).await;

    assert!(exported.iter().any(|name| name == "chat.completion"));
    assert!(exported.iter().any(|name| name == REQUEST_SPAN));
}

#[tokio::test]
async fn the_force_trace_header_keeps_the_whole_trace_at_a_zero_rate() {
    let exported = exported_span_names(
        drop_everything(),
        vec![RequestSpec::llm().header(FORCE_TRACE_HEADER, "1")],
    )
    .await;

    assert!(
        exported.iter().any(|name| name == REQUEST_SPAN),
        "the request span carries the force attribute: {exported:?}"
    );
    assert!(
        exported.iter().any(|name| name == "chat.completion"),
        "and the child span, which ends first, is pinned through the trace id: {exported:?}"
    );
}

#[tokio::test]
async fn a_key_rule_keeps_a_trace_the_default_rate_would_drop() {
    let mut policy = drop_everything();
    policy.rules = vec![rule("key_id", TEST_KEY_ID, 1.0)];

    let exported = exported_span_names(policy, vec![RequestSpec::llm()]).await;

    assert!(
        exported.iter().any(|name| name == REQUEST_SPAN),
        "a rule matching the calling key wins over default_rate: {exported:?}"
    );
}

#[tokio::test]
async fn a_pool_rule_matches_every_key_in_that_pool() {
    let mut policy = drop_everything();
    policy.rules = vec![rule("usage_pool_id", "pool-internal", 1.0)];

    let exported = exported_span_names(
        policy.clone(),
        vec![RequestSpec::llm().in_pool("pool-internal")],
    )
    .await;
    assert!(
        exported.iter().any(|name| name == REQUEST_SPAN),
        "the calling key's pool matched: {exported:?}"
    );

    // A key in another pool, or in none, matches no rule and takes the default.
    for spec in [RequestSpec::llm().in_pool("pool-other"), RequestSpec::llm()] {
        let exported = exported_span_names(policy.clone(), vec![spec]).await;
        assert!(
            exported.is_empty(),
            "only that pool's keys match the rule: {exported:?}"
        );
    }
}

#[tokio::test]
async fn rule_order_decides_which_of_two_matching_rules_applies() {
    // Both rules match: the request carries the tag *and* is made with the key.
    let keep_by_tag = rule("request_tag", "moderation_check", 1.0);
    let drop_by_key = rule("key_id", TEST_KEY_ID, 0.0);
    let request = || RequestSpec::llm().header(REQUEST_TAG_HEADER, "moderation_check");

    let mut tag_first = drop_everything();
    tag_first.rules = vec![keep_by_tag.clone(), drop_by_key.clone()];
    let exported = exported_span_names(tag_first, vec![request()]).await;
    assert!(
        exported.iter().any(|name| name == REQUEST_SPAN),
        "the first matching rule keeps the trace: {exported:?}"
    );

    let mut key_first = drop_everything();
    key_first.rules = vec![drop_by_key, keep_by_tag];
    let exported = exported_span_names(key_first, vec![request()]).await;
    assert!(
        exported.is_empty(),
        "swapping the rules swaps the outcome — position is the whole precedence: {exported:?}"
    );
}

#[tokio::test]
async fn an_errored_response_keeps_its_trace_at_a_zero_rate() {
    let exported = exported_spans(drop_everything(), vec![RequestSpec::llm_failing()]).await;
    let names = exported.names();

    let request_span = exported
        .span(MODERATIONS_REQUEST_SPAN)
        .unwrap_or_else(|| panic!("an errored request must export its trace: {names:?}"));
    assert_eq!(
        bool_attribute(request_span, SAMPLING_FORCE_KEEP_ATTRIBUTE),
        Some(true),
        "kept through the force-keep mechanism, not by the rate"
    );

    // A successful request on the same route and policy is dropped, so the
    // export above is the error status doing the work and nothing else.
    let mut success = RequestSpec::llm();
    success.expected_status = StatusCode::OK;
    let dropped = exported_span_names(drop_everything(), vec![success]).await;
    assert!(
        dropped.is_empty(),
        "the rate still drops a success: {dropped:?}"
    );

    // The handler's own span ended before the response status was known, so the
    // trace-id pin could not reach it: an error trace kept this way starts at
    // the request span. Spans still open when the response is returned — a
    // streaming body's — are pinned.
    assert!(
        !names.iter().any(|name| name == "moderation.create"),
        "documented limit of a response-time keep: {names:?}"
    );
}

#[tokio::test]
async fn a_rule_can_suppress_content_while_its_trace_is_still_exported() {
    openai_server::handlers::init_content_logging(true);
    let mut policy = SamplingPolicy {
        enabled: true,
        default_rate: 1.0,
        default_log_content: true,
        ..SamplingPolicy::default()
    };
    policy.rules = vec![SamplingRule {
        request_tag: None,
        key_id: Some(TEST_KEY_ID.to_string()),
        usage_pool_id: None,
        rate: 1.0,
        log_content: false,
    }];

    let exported = exported_spans(
        policy.clone(),
        vec![RequestSpec::llm_reporting_content_logging()],
    )
    .await;
    let span = exported
        .span("embedding.create")
        .expect("the trace is kept: only its content is suppressed");
    assert_eq!(
        bool_attribute(span, CONTENT_LOGGING_PROBE),
        Some(false),
        "the rule's log_content has to reach the handler writing the span"
    );

    // The same rule with content logging on, to show the probe tracks the rule
    // rather than reporting a constant.
    policy.rules[0].log_content = true;
    let exported = exported_spans(policy, vec![RequestSpec::llm_reporting_content_logging()]).await;
    assert_eq!(
        bool_attribute(
            exported.span("embedding.create").expect("kept"),
            CONTENT_LOGGING_PROBE
        ),
        Some(true)
    );
}

#[tokio::test]
async fn content_logging_is_independent_of_the_keep_decision() {
    // A rule that logs content but drops the trace: the two answers come off one
    // rule match and do not constrain each other.
    let mut policy = SamplingPolicy {
        enabled: true,
        default_rate: 1.0,
        ..SamplingPolicy::default()
    };
    policy.rules = vec![SamplingRule {
        request_tag: None,
        key_id: Some(TEST_KEY_ID.to_string()),
        usage_pool_id: None,
        rate: 0.0,
        log_content: true,
    }];

    let exported =
        exported_span_names(policy, vec![RequestSpec::llm_reporting_content_logging()]).await;
    assert!(
        exported.is_empty(),
        "a content-logging rule still thins its trace away: {exported:?}"
    );
}

#[tokio::test]
async fn both_destinations_reach_the_same_keep_decision() {
    // `main` gates the OTLP pipeline and the Braintrust processor with one
    // shared gate; a destination registered without it would export every
    // recorded span whatever the policy said.
    for (policy, label) in [
        (drop_everything(), "a zero rate"),
        (SamplingPolicy::default(), "the off state"),
    ] {
        let exported = exported_spans(
            policy,
            vec![
                RequestSpec::llm(),
                RequestSpec::llm_with_untagged_child(),
                RequestSpec::llm_failing(),
                RequestSpec::at("/v1/admin/keys"),
            ],
        )
        .await;

        let primary: Vec<_> = exported
            .primary
            .iter()
            .map(|span| (span.name.to_string(), span.span_context.span_id()))
            .collect();
        let secondary: Vec<_> = exported
            .secondary
            .iter()
            .map(|span| (span.name.to_string(), span.span_context.span_id()))
            .collect();
        assert_eq!(
            primary, secondary,
            "the two gated destinations disagreed under {label}"
        );
        assert!(!primary.is_empty(), "{label} should export something");
    }
}

#[tokio::test]
async fn the_decision_attribute_never_reaches_an_exporter() {
    // Internal to the gate: always `true` on anything that got exported, so it
    // would cost an attribute slot to say nothing. The request tag and the
    // force-keep flag do carry information and stay.
    let exported = exported_spans(
        SamplingPolicy::default(),
        vec![
            RequestSpec::llm().header(REQUEST_TAG_HEADER, "openai_complete"),
            RequestSpec::llm_failing(),
        ],
    )
    .await;

    let keys = exported.attribute_keys();
    for stripped in [SAMPLING_DECISION_ATTRIBUTE, SAMPLING_LOG_CONTENT_ATTRIBUTE] {
        assert!(
            !keys.iter().any(|key| key == stripped),
            "{stripped} must not be exported: {keys:?}"
        );
    }
    assert!(
        keys.iter().any(|key| key == CALLER_REQUEST_TAG_ATTRIBUTE),
        "the caller's request tag is worth exporting: {keys:?}"
    );
    assert!(
        keys.iter().any(|key| key == SAMPLING_FORCE_KEEP_ATTRIBUTE),
        "and so is the reason a trace was pinned: {keys:?}"
    );
}

#[tokio::test]
async fn a_request_tag_rule_thins_a_trace_the_default_rate_would_keep() {
    let mut policy = SamplingPolicy {
        enabled: true,
        default_rate: 1.0,
        ..SamplingPolicy::default()
    };
    policy.rules = vec![rule("request_tag", "noisy_probe", 0.0)];

    let exported = exported_span_names(
        policy,
        vec![RequestSpec::llm().header(REQUEST_TAG_HEADER, "noisy_probe")],
    )
    .await;

    assert!(
        exported.is_empty(),
        "the tag's rate must thin the trace the default rate would have kept: {exported:?}"
    );
}

#[tokio::test]
async fn one_conversation_gets_one_decision_across_requests() {
    // At rate 0.5 the two requests below hash to opposite answers on their own
    // trace ids (they are distinct traces), so agreeing can only come from the
    // session key.
    let policy = SamplingPolicy {
        enabled: true,
        default_rate: 0.5,
        ..SamplingPolicy::default()
    };
    for session_id in ["ses-a", "ses-b", "ses-c", "ses-d"] {
        let exported = exported_span_names(
            policy.clone(),
            vec![
                RequestSpec::llm().header("baggage", format!("session.id={session_id}")),
                RequestSpec::llm().header("baggage", format!("session.id={session_id}")),
            ],
        )
        .await;

        let request_spans = exported.iter().filter(|name| *name == REQUEST_SPAN).count();
        assert!(
            request_spans == 0 || request_spans == 2,
            "session {session_id} split its conversation: {exported:?}"
        );
    }
}

#[tokio::test]
async fn an_outcome_rate_overrides_the_rate_based_decision_in_either_direction() {
    // default_rate 0.0 would drop everything; an outcome rate of 1.0 must
    // still keep this trace.
    let drop_everything = SamplingPolicy {
        enabled: true,
        default_rate: 0.0,
        ..SamplingPolicy::default()
    };
    let exported = exported_span_names(
        drop_everything,
        vec![RequestSpec::llm_outcome().header("x-test-outcome-rate", "1.0")],
    )
    .await;
    assert!(
        exported.contains(&OUTCOME_REQUEST_SPAN.to_string()),
        "an outcome rate of 1.0 must keep a trace the default rate would have dropped: {exported:?}"
    );

    // default_rate 1.0 would keep everything; an outcome rate of 0.0 must
    // still drop this trace.
    let keep_everything = SamplingPolicy {
        enabled: true,
        default_rate: 1.0,
        ..SamplingPolicy::default()
    };
    let exported = exported_span_names(
        keep_everything,
        vec![RequestSpec::llm_outcome().header("x-test-outcome-rate", "0.0")],
    )
    .await;
    assert!(
        exported.is_empty(),
        "an outcome rate of 0.0 must drop a trace the default rate would have kept: {exported:?}"
    );
}

#[tokio::test]
async fn force_keep_still_wins_over_an_outcome_rate_that_resolved_to_drop() {
    let keep_everything = SamplingPolicy {
        enabled: true,
        default_rate: 1.0,
        ..SamplingPolicy::default()
    };
    let exported = exported_span_names(
        keep_everything,
        vec![RequestSpec::llm_outcome()
            .header("x-test-outcome-rate", "0.0")
            .header("x-test-force-keep", "1")],
    )
    .await;
    assert!(
        exported.contains(&OUTCOME_REQUEST_SPAN.to_string()),
        "an unconditional force-keep must win over an outcome rate that resolved to drop: {exported:?}"
    );
}

#[tokio::test]
async fn a_live_http_span_retains_its_child_outcome_after_history_saturates() {
    // No outcome rules existed at request entry; the decision token must still
    // be present if a newly enabled rule matches during this request.
    let exported = exported_spans(
        drop_everything(),
        vec![RequestSpec::llm().header("x-test-saturate-outcomes", "true")],
    )
    .await;
    assert!(exported.span("chat.completion").is_some());
    assert!(exported.span(REQUEST_SPAN).is_some());
    assert_eq!(exported.primary.len(), exported.secondary.len());
    assert_eq!(exported.primary.len(), 2);
}
