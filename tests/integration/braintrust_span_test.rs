//! End-to-end coverage for the Braintrust span pipeline.
//!
//! Two halves, matching the two pieces of `telemetry::braintrust`:
//!
//! * the **filter** — a request to an LLM frontend puts exactly its request
//!   span and its LLM span on the Braintrust processor, and a request to
//!   anything else puts nothing there, even though both are still exported to
//!   Chalk's collector; and
//! * the **exporter** — the bytes it POSTs are a decodable OTLP payload carrying
//!   those spans' OpenInference attributes, addressed to the configured project
//!   with the credentials Braintrust expects.

use std::sync::{Arc, Mutex};

use axum::{
    body::Body,
    extract::State,
    http::{self, HeaderMap, Request, StatusCode},
    routing::post,
    Router,
};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_sdk::export::trace::{SpanData, SpanExporter};
use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
use opentelemetry_sdk::trace::{SimpleSpanProcessor, TracerProvider};
use opentelemetry_sdk::Resource;
use prost::Message;
use tower::ServiceExt;
use tower_http::trace::TraceLayer;
use tracing_subscriber::layer::SubscriberExt;

use openai_server::telemetry::braintrust::{
    BraintrustConfig, BraintrustConfigState, BraintrustSpanExporter, BraintrustSpanProcessor,
};
use openai_server::telemetry::sampling::SamplingGate;
use openai_server::telemetry::OtelMakeSpan;

use super::span_test::MockBackend;
use crate::support::app::{build_full_app, chat_request_body, TEST_API_KEY};
use crate::support::otel::span_attrs;

/// The router's own HTTP surface, wrapped in the same tracing layer `main.rs`
/// installs — the layer is where a request span learns which frontend it is.
async fn traced_app() -> Router {
    build_full_app(Arc::new(MockBackend))
        .await
        // A gate with no policy store keeps every span, so this suite exercises
        // the Braintrust filter rather than the sampling policy.
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(OtelMakeSpan::new(Arc::new(SamplingGate::new()))),
        )
}

async fn sample_frontend_spans() -> Vec<SpanData> {
    let (provider, _all, braintrust, subscriber) = setup_otel();
    let app = traced_app().await;
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .header(
                        http::header::AUTHORIZATION,
                        format!("Bearer {TEST_API_KEY}"),
                    )
                    .body(Body::from(chat_request_body(false).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    provider.force_flush();
    braintrust.get_finished_spans().unwrap()
}

/// A tracer provider with two sinks: everything (what Chalk's collector sees)
/// and the Braintrust-filtered subset.
fn setup_otel() -> (
    TracerProvider,
    InMemorySpanExporter,
    InMemorySpanExporter,
    impl tracing::Subscriber + Send + Sync,
) {
    let all = InMemorySpanExporter::default();
    let braintrust = InMemorySpanExporter::default();
    let configs = Arc::new(BraintrustConfigState::new(vec![BraintrustConfig {
        traces_url: "http://unused.test/otel/v1/traces".to_string(),
        api_key: "unused".to_string(),
        parent: "project_name:test".to_string(),
    }]));

    let provider = TracerProvider::builder()
        .with_span_processor(SimpleSpanProcessor::new(Box::new(all.clone())))
        .with_span_processor(BraintrustSpanProcessor::new(
            SimpleSpanProcessor::new(Box::new(braintrust.clone())),
            configs,
        ))
        .build();

    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));

    (provider, all, braintrust, subscriber)
}

fn find<'a>(spans: &'a [SpanData], name: &str) -> Option<&'a SpanData> {
    spans.iter().find(|s| s.name == name)
}

// ---------------------------------------------------------------------------
// The filter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn frontend_request_exports_its_request_and_llm_spans() {
    let (provider, all, braintrust, subscriber) = setup_otel();
    let app = traced_app().await;

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .header(
                        http::header::AUTHORIZATION,
                        format!("Bearer {TEST_API_KEY}"),
                    )
                    .body(Body::from(chat_request_body(false).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    provider.force_flush();

    let exported = braintrust.get_finished_spans().unwrap();
    let names: Vec<&str> = exported.iter().map(|s| s.name.as_ref()).collect();
    assert_eq!(
        names.len(),
        2,
        "expected exactly the request span and the LLM span, got {names:?}"
    );

    let request_span = find(&exported, "POST /v1/chat/completions")
        .expect("the frontend's request span reaches Braintrust as the trace root");
    let request_attrs = span_attrs(request_span);
    assert_eq!(
        request_attrs.get("chalk.frontend").map(String::as_str),
        Some("openai")
    );
    assert_eq!(
        request_attrs
            .get("chalk.frontend.endpoint")
            .map(String::as_str),
        Some("chat.completions")
    );

    let llm_span = find(&exported, "chat.completion").expect("the LLM span reaches Braintrust");
    assert_eq!(
        span_attrs(llm_span)
            .get("openinference.span.kind")
            .map(String::as_str),
        Some("LLM")
    );

    // Braintrust nests a trace from these ids, so the link has to survive the
    // filter: same trace, LLM span parented to the request span.
    assert_eq!(
        llm_span.span_context.trace_id(),
        request_span.span_context.trace_id()
    );
    assert_eq!(llm_span.parent_span_id, request_span.span_context.span_id());

    // The unfiltered pipeline is unaffected by any of this.
    assert!(all.get_finished_spans().unwrap().len() >= exported.len());
}

#[tokio::test]
async fn anthropic_frontend_is_labeled_by_its_own_dialect() {
    let (provider, _all, braintrust, subscriber) = setup_otel();
    // `/v1/messages` is not mounted on the shared test app, so this exercises
    // the layer alone: what matters is the label the path earns, which is set
    // before routing and therefore before the 404.
    let app = traced_app().await;

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let _ = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .header(
                        http::header::AUTHORIZATION,
                        format!("Bearer {TEST_API_KEY}"),
                    )
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
    }
    provider.force_flush();

    let exported = braintrust.get_finished_spans().unwrap();
    let request_span = find(&exported, "POST /v1/messages").expect("request span exported");
    assert_eq!(
        span_attrs(request_span)
            .get("chalk.frontend")
            .map(String::as_str),
        Some("anthropic")
    );
}

#[tokio::test]
async fn non_frontend_requests_export_nothing() {
    for (path, should_trace) in [
        ("/v1/admin/keys", true),
        ("/v1/models", true),
        ("/health", false),
        ("/v1/healthcheck", false),
        ("/grpc.health.v1.Health/Check", false),
        ("/grpc.health.v1.Health/Watch", false),
    ] {
        let (provider, all, braintrust, subscriber) = setup_otel();
        let app = traced_app().await;

        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let _ = app
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(path)
                        .header(
                            http::header::AUTHORIZATION,
                            format!("Bearer {TEST_API_KEY}"),
                        )
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
        }
        provider.force_flush();

        // Management requests remain traced, while probes reach no exporter.
        assert_eq!(
            !all.get_finished_spans().unwrap().is_empty(),
            should_trace,
            "unexpected tracing for {path}"
        );
        assert!(
            braintrust.get_finished_spans().unwrap().is_empty(),
            "{path} must not reach Braintrust"
        );
    }
}

// ---------------------------------------------------------------------------
// The exporter
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Captured {
    headers: Arc<Mutex<Option<HeaderMap>>>,
    body: Arc<Mutex<Option<bytes::Bytes>>>,
}

async fn capture(
    State(captured): State<Captured>,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> StatusCode {
    *captured.headers.lock().unwrap() = Some(headers);
    *captured.body.lock().unwrap() = Some(body);
    StatusCode::OK
}

/// Run a stub OTLP collector, returning its base URL and the capture slot.
async fn stub_collector() -> (String, Captured) {
    let captured = Captured::default();
    let app = Router::new()
        .route("/otel/v1/traces", post(capture))
        .with_state(captured.clone());

    let base = crate::support::mock::serve(app).await;

    (base, captured)
}

#[tokio::test]
async fn exports_otlp_protobuf_addressed_to_the_configured_project() {
    // Produce a real LLM span to export, rather than a synthetic one.
    let (provider, _all, braintrust, subscriber) = setup_otel();
    let app = traced_app().await;
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let _ = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .header(
                        http::header::AUTHORIZATION,
                        format!("Bearer {TEST_API_KEY}"),
                    )
                    .body(Body::from(chat_request_body(false).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
    }
    provider.force_flush();
    let spans = braintrust.get_finished_spans().unwrap();
    assert!(!spans.is_empty());

    let (base, captured) = stub_collector().await;
    let config = BraintrustConfig {
        traces_url: format!("{base}/otel/v1/traces"),
        api_key: "sk-test-key".to_string(),
        parent: "project_name:chalk-router-test".to_string(),
    };

    let mut exporter = BraintrustSpanExporter::new(
        Arc::new(BraintrustConfigState::new(vec![config])),
        &Resource::empty(),
    );
    exporter.export(spans).await.expect("export succeeds");

    let headers = captured
        .headers
        .lock()
        .unwrap()
        .clone()
        .expect("collector was called");
    assert_eq!(headers["authorization"], "Bearer sk-test-key");
    assert_eq!(headers["x-bt-parent"], "project_name:chalk-router-test");
    assert_eq!(headers["content-type"], "application/x-protobuf");

    let body = captured.body.lock().unwrap().clone().unwrap();
    let decoded = ExportTraceServiceRequest::decode(body).expect("body is an OTLP trace request");
    let exported: Vec<_> = decoded
        .resource_spans
        .iter()
        .flat_map(|rs| rs.scope_spans.iter())
        .flat_map(|ss| ss.spans.iter())
        .collect();

    let llm = exported
        .iter()
        .find(|s| s.name == "chat.completion")
        .expect("the LLM span survived encoding");
    let kind = llm
        .attributes
        .iter()
        .find(|kv| kv.key == "openinference.span.kind")
        .and_then(|kv| kv.value.as_ref())
        .and_then(|v| v.value.clone());
    assert!(
        matches!(kind, Some(opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(ref s)) if s == "LLM"),
        "OpenInference attributes are what Braintrust reads: {kind:?}"
    );
}

#[tokio::test]
async fn exports_each_batch_to_every_configured_braintrust_destination() {
    let (first_base, first_capture) = stub_collector().await;
    let (second_base, second_capture) = stub_collector().await;
    let configs = vec![
        BraintrustConfig {
            traces_url: format!("{first_base}/otel/v1/traces"),
            api_key: "sk-first".to_string(),
            parent: "project_name:first".to_string(),
        },
        BraintrustConfig {
            traces_url: format!("{second_base}/otel/v1/traces"),
            api_key: "sk-second".to_string(),
            parent: "project_name:second".to_string(),
        },
    ];
    let mut exporter = BraintrustSpanExporter::new(
        Arc::new(BraintrustConfigState::new(configs)),
        &Resource::empty(),
    );
    let spans = sample_frontend_spans().await;
    assert!(!spans.is_empty());

    exporter
        .export(spans)
        .await
        .expect("fanout export succeeds");

    let first_headers = first_capture
        .headers
        .lock()
        .unwrap()
        .clone()
        .expect("first collector was called");
    let second_headers = second_capture
        .headers
        .lock()
        .unwrap()
        .clone()
        .expect("second collector was called");
    assert_eq!(first_headers["authorization"], "Bearer sk-first");
    assert_eq!(first_headers["x-bt-parent"], "project_name:first");
    assert_eq!(second_headers["authorization"], "Bearer sk-second");
    assert_eq!(second_headers["x-bt-parent"], "project_name:second");

    let first_body = first_capture.body.lock().unwrap().clone().unwrap();
    let second_body = second_capture.body.lock().unwrap().clone().unwrap();
    assert_eq!(
        first_body, second_body,
        "destinations receive the same batch"
    );
    let decoded = ExportTraceServiceRequest::decode(first_body).unwrap();
    assert!(decoded
        .resource_spans
        .iter()
        .flat_map(|resource| &resource.scope_spans)
        .flat_map(|scope| &scope.spans)
        .any(|span| span.name == "chat.completion"));
}

#[tokio::test]
async fn one_rejected_destination_does_not_block_the_other_destinations() {
    let (success_base, success_capture) = stub_collector().await;
    let rejected_app = Router::new().route(
        "/otel/v1/traces",
        post(|| async { (StatusCode::FORBIDDEN, "invalid parent") }),
    );
    let rejected_base = crate::support::mock::serve(rejected_app).await;
    let configs = vec![
        BraintrustConfig {
            traces_url: format!("{rejected_base}/otel/v1/traces"),
            api_key: "sk-rejected".to_string(),
            parent: "project_name:rejected".to_string(),
        },
        BraintrustConfig {
            traces_url: format!("{success_base}/otel/v1/traces"),
            api_key: "sk-success".to_string(),
            parent: "project_name:success".to_string(),
        },
    ];
    let mut exporter = BraintrustSpanExporter::new(
        Arc::new(BraintrustConfigState::new(configs)),
        &Resource::empty(),
    );

    let error = exporter
        .export(sample_frontend_spans().await)
        .await
        .expect_err("the aggregate result reports the rejected destination");

    // The body is what tells an operator *why* a destination was rejected —
    // keep it in the message, not just the status code.
    let message = error.to_string();
    assert!(message.contains("403"), "{message}");
    assert!(message.contains("invalid parent"), "{message}");
    let body = success_capture
        .body
        .lock()
        .unwrap()
        .clone()
        .expect("the healthy destination still receives the batch");
    let decoded = ExportTraceServiceRequest::decode(body).unwrap();
    assert!(decoded
        .resource_spans
        .iter()
        .flat_map(|resource| &resource.scope_spans)
        .flat_map(|scope| &scope.spans)
        .any(|span| span.name == "chat.completion"));
}
