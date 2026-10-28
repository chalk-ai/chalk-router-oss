//! Datadog-compatible JSON log formatter.
//!
//! Vendored from Chalk's `chalk_telemetry` crate (via chalk-mcp-gateway), which
//! isn't consumable here today — it lives in chalk-private and pins an older
//! OpenTelemetry, which would mean two disconnected global tracer providers in
//! one binary. This formatter is self-contained (only `tracing-subscriber`,
//! `opentelemetry` for trace IDs, `chrono`, `serde`) and compiles against our
//! own OTel version.
//!
//! Emits one JSON object per event with the field shape Chalk's Python services
//! use (`message`, `level`, `status`, `labels`, OTel `traceId`/`spanId`, …) so
//! router logs facet the same way in Datadog and correlate with APM traces.

#[derive(serde::Serialize)]
struct LogRecord<'a> {
    timestamp: String,
    level: String,
    message: String,
    status: String,
    target: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    service: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    env: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<u32>,
    #[serde(rename = "threadId")]
    thread_id: String,
    #[serde(rename = "threadName")]
    thread_name: String,
    #[serde(rename = "traceId", skip_serializing_if = "Option::is_none")]
    trace_id: Option<String>,
    #[serde(rename = "spanId", skip_serializing_if = "Option::is_none")]
    span_id: Option<String>,
    #[serde(rename = "traceSampled", skip_serializing_if = "Option::is_none")]
    trace_sampled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    labels: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Datadog-compatible JSON event formatter.
#[derive(Debug, Clone)]
pub struct DatadogJsonFormatter {
    service: Option<String>,
    env: Option<String>,
}

impl DatadogJsonFormatter {
    pub fn new(service_name: impl Into<String>, env: Option<String>) -> Self {
        let service_name = service_name.into();
        Self {
            service: (!service_name.is_empty()).then_some(service_name),
            env,
        }
    }
}

impl<S, N> tracing_subscriber::fmt::FormatEvent<S, N> for DatadogJsonFormatter
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let metadata = event.metadata();

        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);
        let level = if is_otlp_span_export_error(event, &visitor) {
            tracing::Level::WARN
        } else {
            *metadata.level()
        };

        use opentelemetry::trace::TraceContextExt;
        let otel_context = opentelemetry::Context::current();
        let span_context = otel_context.span().span_context().clone();
        let (trace_id, span_id, trace_sampled) = if span_context.is_valid() {
            (
                Some(span_context.trace_id().to_string()),
                Some(span_context.span_id().to_string()),
                Some(span_context.is_sampled()),
            )
        } else {
            (None, None, None)
        };

        let mut labels = serde_json::Map::new();
        for (key, value) in visitor.fields.iter() {
            labels.insert(key.clone(), serde_json::json!(value));
        }

        let record = LogRecord {
            timestamp: chrono::Utc::now().to_rfc3339(),
            level: level.to_string(),
            message: visitor.message,
            status: level.to_string().to_lowercase(),
            target: metadata.target(),
            service: self.service.as_deref(),
            env: self.env.as_deref(),
            file: metadata.file(),
            line: metadata.line(),
            thread_id: format!("{:?}", std::thread::current().id()),
            thread_name: std::thread::current()
                .name()
                .unwrap_or("unknown")
                .to_string(),
            trace_id,
            span_id,
            trace_sampled,
            labels: (!labels.is_empty()).then_some(labels),
        };

        let serialized = serde_json::to_string(&record).map_err(|_| std::fmt::Error)?;
        writeln!(writer, "{serialized}")
    }
}

/// The OTLP exporter marks its terminal errors so the shared SDK processor's
/// logs can be distinguished from other span exporters' failures.
fn is_otlp_span_export_error(event: &tracing::Event<'_>, visitor: &JsonVisitor) -> bool {
    let metadata = event.metadata();
    metadata.level() == &tracing::Level::ERROR
        && metadata.target() == "opentelemetry_sdk"
        && matches!(
            metadata.name(),
            "BatchSpanProcessor.Flush.ExportError" | "BatchSpanProcessor.Export.Error"
        )
        && visitor
            .fields
            .get("reason")
            .is_some_and(|reason| reason.contains(super::otlp_retry::OTLP_EXPORT_ERROR_MARKER))
}

/// Local pretty logs use the same severity as the production JSON formatter.
#[derive(Default)]
pub struct PrettyLogFormatter;

impl<S, N> tracing_subscriber::fmt::FormatEvent<S, N> for PrettyLogFormatter
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);
        if is_otlp_span_export_error(event, &visitor) {
            writeln!(
                writer,
                "{}  WARN {}: {}: {}",
                chrono::Utc::now().to_rfc3339(),
                event.metadata().target(),
                event.metadata().name(),
                visitor.fields["reason"]
            )
        } else {
            tracing_subscriber::fmt::format().format_event(ctx, writer, event)
        }
    }
}

#[derive(Default)]
struct JsonVisitor {
    message: String,
    fields: std::collections::HashMap<String, String>,
}

impl tracing::field::Visit for JsonVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}").trim_matches('"').to_string();
        } else {
            self.fields.insert(
                field.name().to_string(),
                format!("{value:?}").trim_matches('"').to_string(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    use super::*;

    #[derive(Clone)]
    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriterGuard;
        fn make_writer(&'a self) -> Self::Writer {
            CaptureWriterGuard(self.0.clone())
        }
    }

    struct CaptureWriterGuard(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriterGuard {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn emit_and_capture(f: impl FnOnce()) -> serde_json::Value {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let layer = tracing_subscriber::fmt::layer()
            .event_format(DatadogJsonFormatter::new("chalk-router", None))
            .with_writer(CaptureWriter(buf.clone()));
        let _guard = tracing_subscriber::registry().with(layer).set_default();
        f();
        let output = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        serde_json::from_str(output.trim()).expect("formatter emitted valid JSON")
    }

    #[test]
    fn emits_core_fields() {
        let json = emit_and_capture(|| tracing::info!("hello"));
        assert_eq!(json["level"], "INFO");
        assert_eq!(json["status"], "info");
        assert_eq!(json["message"], "hello");
        assert_eq!(json["service"], "chalk-router");
        assert!(json.get("timestamp").is_some());
    }

    #[test]
    fn structured_fields_go_to_labels() {
        let json = emit_and_capture(|| tracing::warn!(backend = "bigquery", "denied"));
        assert_eq!(json["labels"]["backend"], "bigquery");
    }

    #[test]
    fn otlp_flush_export_failure_is_a_warning() {
        let json = emit_and_capture(|| {
            tracing::error!(
                name: "BatchSpanProcessor.Flush.ExportError",
                target: "opentelemetry_sdk",
                name = "BatchSpanProcessor.Flush.ExportError",
                reason = "ExportFailed(OTLP span export error: ExportFailed(Status { code: Unavailable }))",
                message = "Failed during the export process",
                ""
            );
        });
        assert_eq!(json["level"], "WARN");
        assert_eq!(json["status"], "warn");
        assert_eq!(
            json["labels"]["name"],
            "BatchSpanProcessor.Flush.ExportError"
        );
    }

    #[test]
    fn otlp_non_retryable_export_failure_is_a_warning() {
        let json = emit_and_capture(|| {
            tracing::error!(
                name: "BatchSpanProcessor.Export.Error",
                target: "opentelemetry_sdk",
                name = "BatchSpanProcessor.Export.Error",
                reason = "Exporter otlp encountered the following error(s): OTLP span export error: Exporter otlp encountered PermissionDenied",
                ""
            );
        });
        assert_eq!(json["level"], "WARN");
        assert_eq!(json["status"], "warn");
    }

    #[test]
    fn otlp_timeout_is_a_warning() {
        let json = emit_and_capture(|| {
            tracing::error!(
                name: "BatchSpanProcessor.Flush.ExportError",
                target: "opentelemetry_sdk",
                name = "BatchSpanProcessor.Flush.ExportError",
                reason = "ExportFailed(OTLP span export error: ExportTimedOut(300s))",
                ""
            );
        });
        assert_eq!(json["level"], "WARN");
    }

    #[test]
    fn other_exporters_failures_remain_errors() {
        let json = emit_and_capture(|| {
            tracing::error!(
                name: "BatchSpanProcessor.Flush.ExportError",
                target: "opentelemetry_sdk",
                name = "BatchSpanProcessor.Flush.ExportError",
                reason = "Exporter braintrust encountered the following error(s): PermissionDenied",
                ""
            );
        });
        assert_eq!(json["level"], "ERROR");
        assert_eq!(json["status"], "error");
    }

    #[test]
    fn pretty_logs_downgrade_otlp_export_failure() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let layer = tracing_subscriber::fmt::layer()
            .event_format(PrettyLogFormatter)
            .with_writer(CaptureWriter(buf.clone()));
        let _guard = tracing_subscriber::registry().with(layer).set_default();
        tracing::error!(
            name: "BatchSpanProcessor.Flush.ExportError",
            target: "opentelemetry_sdk",
            name = "BatchSpanProcessor.Flush.ExportError",
            reason = "ExportFailed(OTLP span export error: ExportFailed(Status { code: PermissionDenied }))",
            ""
        );
        let output = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(output.contains("WARN opentelemetry_sdk:"), "{output}");
        assert!(!output.contains("ERROR opentelemetry_sdk:"), "{output}");
    }
}
