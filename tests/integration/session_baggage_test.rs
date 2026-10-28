//! A caller's session id reaches the router as W3C `baggage`, and has to land on its spans.
//!
//! Chalk groups a trace into an agent session by the `session.id` *attribute* of its spans,
//! so a session id that only ever lives in the request context is invisible: the router's
//! `chat.completion` span carries the token counts, and without this it carried no session.
//! There is no field on a completion request to put it in, and there should not be — one
//! conversation spans many requests, so it belongs beside the trace context.

use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::post,
    Router,
};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
use opentelemetry_sdk::trace::{SimpleSpanProcessor, TracerProvider};
use tower::ServiceExt;
use tower_http::trace::TraceLayer;
use tracing_subscriber::layer::SubscriberExt;

use openai_server::telemetry::sampling::SamplingGate;
use openai_server::telemetry::{
    text_map_propagator, OtelMakeSpan, SessionIdSpanProcessor, SESSION_ID_ATTRIBUTE,
};

const TRACE_ID: &str = "0af7651916cd43dd8448eb211c80319c";
const SPAN_ID: &str = "b7ad6b7169203331";

/// Stands in for `create_chat_completion`: a span opened inside the handler, well away from
/// the layer that extracted the context.
async fn handler() -> StatusCode {
    let span = tracing::info_span!("chat_completion", otel.name = "chat.completion");
    let _entered = span.enter();
    StatusCode::OK
}

fn app() -> Router {
    Router::new()
        .route("/v1/chat/completions", post(handler))
        // A gate with no policy store keeps every span; this suite is about the
        // session id reaching them, not about sampling.
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(OtelMakeSpan::new(std::sync::Arc::new(SamplingGate::new()))),
        )
}

/// Drive one request through the real layer stack and return every exported span's
/// `session.id`, keyed by span name.
async fn session_ids_for(headers: Vec<(&str, &str)>) -> Vec<(String, Option<String>)> {
    let exporter = InMemorySpanExporter::default();
    let provider = TracerProvider::builder()
        // Order matters only in that both see on_start; the exporter reports at on_end, by
        // which time the stamp is on the span.
        .with_span_processor(SessionIdSpanProcessor)
        .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
        .build();
    opentelemetry::global::set_text_map_propagator(text_map_propagator());

    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));

    {
        // set_default installs the subscriber for this thread only, so the request has to be
        // driven with oneshot rather than a spawned server.
        let _guard = tracing::subscriber::set_default(subscriber);
        let mut request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json");
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = app()
            .oneshot(request.body(Body::from("{}")).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    exporter
        .get_finished_spans()
        .unwrap()
        .into_iter()
        .map(|span| {
            let session = span
                .attributes
                .iter()
                .find(|kv| kv.key.as_str() == SESSION_ID_ATTRIBUTE)
                .map(|kv| kv.value.to_string());
            (span.name.to_string(), session)
        })
        .collect()
}

#[tokio::test]
async fn baggage_places_every_span_of_the_request_in_the_callers_session() {
    let spans = session_ids_for(vec![
        ("traceparent", &format!("00-{TRACE_ID}-{SPAN_ID}-01")),
        ("baggage", "session.id=ses-42"),
    ])
    .await;

    assert!(!spans.is_empty(), "expected the request to export spans");
    for (name, session) in &spans {
        assert_eq!(
            session.as_deref(),
            Some("ses-42"),
            "span {name} was not placed in the session"
        );
    }
    // The handler's own span is the one that carries token counts in production, so it is
    // the one that must not be missed.
    assert!(spans.iter().any(|(name, _)| name == "chat.completion"));
}

#[tokio::test]
async fn baggage_is_honored_without_a_traceparent() {
    // The two headers are independent: a caller that starts no trace of its own can still
    // name the conversation its request belongs to.
    let spans = session_ids_for(vec![("baggage", "session.id=ses-alone")]).await;

    assert!(!spans.is_empty());
    for (name, session) in &spans {
        assert_eq!(session.as_deref(), Some("ses-alone"), "span {name}");
    }
}

#[tokio::test]
async fn a_request_without_baggage_stamps_nothing() {
    let spans = session_ids_for(vec![(
        "traceparent",
        &format!("00-{TRACE_ID}-{SPAN_ID}-01"),
    )])
    .await;

    assert!(!spans.is_empty());
    for (name, session) in &spans {
        assert_eq!(session, &None, "span {name} invented a session");
    }
}

#[tokio::test]
async fn other_baggage_entries_do_not_become_a_session() {
    let spans = session_ids_for(vec![("baggage", "userId=alice,region=us-east-1")]).await;

    assert!(!spans.is_empty());
    for (name, session) in &spans {
        assert_eq!(session, &None, "span {name} invented a session");
    }
}
