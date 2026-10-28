//! OpenTelemetry test scaffolding, and the `ANTHROPIC_API_BASE` test mutex.

use std::collections::HashMap;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
use opentelemetry_sdk::trace::{SimpleSpanProcessor, TracerProvider};
use tracing_subscriber::layer::SubscriberExt;

/// Build an in-memory span exporter plus a `tracing` subscriber that feeds it,
/// for tests asserting on exported span attributes. The returned subscriber
/// is not installed globally — pass it to `tracing::subscriber::with_default`
/// around the code under test.
pub fn setup_otel() -> (
    TracerProvider,
    InMemorySpanExporter,
    impl tracing::Subscriber + Send + Sync,
) {
    // Content assertions deliberately opt in; production defaults remain private.
    openai_server::handlers::init_content_logging(true);
    let exporter = InMemorySpanExporter::default();
    let provider = TracerProvider::builder()
        .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
        .build();

    let telemetry_layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("test"));

    let subscriber = tracing_subscriber::registry().with(telemetry_layer);

    (provider, exporter, subscriber)
}

/// Flatten a span's attributes into a `key -> stringified value` map for easy
/// assertions.
pub fn span_attrs(span: &opentelemetry_sdk::export::trace::SpanData) -> HashMap<String, String> {
    span.attributes
        .iter()
        .map(|kv| (kv.key.to_string(), kv.value.to_string()))
        .collect()
}

/// `anthropic_span_test` is the only integration test today that takes this
/// lock (see `anthropic_base_lock` below) before starting its mock Anthropic
/// upstream. Nothing here actually mutates a process-global
/// `ANTHROPIC_API_BASE` — `ANTHROPIC_API_BASE` is a private `const` fallback
/// in `src/backend/anthropic.rs`, and every test instead passes its mock's
/// base URL explicitly through an `AppState`/provider-connection override —
/// so this lock currently has no other holder to race with. It stays in case
/// a future test needs to serialize on a real shared global; if you're
/// reaching for it, confirm there's still something to protect.
static ANTHROPIC_BASE_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the `ANTHROPIC_API_BASE` lock. Poisoning is ignored: a panicking test
/// has already failed, and blocking every other test behind it helps nobody.
pub fn anthropic_base_lock() -> std::sync::MutexGuard<'static, ()> {
    ANTHROPIC_BASE_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
