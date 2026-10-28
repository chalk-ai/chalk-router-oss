//! Retry the built-in OTLP exporter while retaining the original span batch.
//!
//! opentelemetry-otlp 0.27 sends once, and the SDK removes a batch from its
//! bounded queue before calling the exporter. A failed request otherwise loses
//! that batch even when the collector becomes ready moments later.

use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use opentelemetry::trace::{ExportError, TraceError};
use opentelemetry_otlp::Error as OtlpError;
use opentelemetry_sdk::export::trace::{ExportResult, SpanData, SpanExporter};
use opentelemetry_sdk::Resource;
use tokio::time::{sleep, timeout, Instant};
use tonic::Code;

pub(super) const RPC_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const RETRY_WINDOW: Duration = Duration::from_secs(300);
pub(super) const PROCESSOR_TIMEOUT: Duration = RETRY_WINDOW.saturating_add(RPC_TIMEOUT);
pub(super) const OTLP_EXPORT_ERROR_MARKER: &str = "OTLP span export error";

struct OtlpBatchError(TraceError);

impl fmt::Debug for OtlpBatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{OTLP_EXPORT_ERROR_MARKER}: {:?}", self.0)
    }
}

impl fmt::Display for OtlpBatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{OTLP_EXPORT_ERROR_MARKER}: {}", self.0)
    }
}

impl Error for OtlpBatchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

impl ExportError for OtlpBatchError {
    fn exporter_name(&self) -> &'static str {
        "otlp"
    }
}

#[derive(Clone, Copy, Debug)]
struct RetryPolicy {
    max_elapsed: Duration,
    initial_backoff: Duration,
    max_backoff: Duration,
    attempt_timeout: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_elapsed: RETRY_WINDOW,
            initial_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(5),
            attempt_timeout: RPC_TIMEOUT,
        }
    }
}

#[derive(Debug)]
pub(super) struct RetryingSpanExporter {
    inner: Arc<Mutex<Box<dyn SpanExporter>>>,
    shutdown: Arc<AtomicBool>,
    policy: RetryPolicy,
    resource: Resource,
}

impl RetryingSpanExporter {
    pub(super) fn new(exporter: impl SpanExporter + 'static) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Box::new(exporter))),
            shutdown: Arc::new(AtomicBool::new(false)),
            policy: RetryPolicy::default(),
            resource: Resource::default(),
        }
    }

    #[cfg(test)]
    fn with_policy(exporter: impl SpanExporter + 'static, policy: RetryPolicy) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Box::new(exporter))),
            shutdown: Arc::new(AtomicBool::new(false)),
            policy,
            resource: Resource::default(),
        }
    }
}

impl SpanExporter for RetryingSpanExporter {
    fn export(&mut self, batch: Vec<SpanData>) -> BoxFuture<'static, ExportResult> {
        let inner = Arc::clone(&self.inner);
        let shutdown = Arc::clone(&self.shutdown);
        let policy = self.policy;
        let resource = self.resource.clone();
        Box::pin(async move {
            let started = Instant::now();
            let mut backoff = policy.initial_backoff;
            let mut attempts = 0;
            loop {
                if shutdown.load(Ordering::Acquire) {
                    let error = TraceError::from("OTLP exporter shut down during export");
                    return Err(log_drop(&batch, attempts, error, "exporter shutdown"));
                }
                let remaining = policy.max_elapsed.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    // This is reachable only after a previous retryable failure.
                    let error = TraceError::ExportTimedOut(policy.max_elapsed);
                    return Err(log_drop(&batch, attempts, error, "retry window exhausted"));
                }

                attempts += 1;
                // The 0.27 exporter consumes its input before the RPC completes.
                // Keep the original batch alive until an attempt succeeds or the
                // retry window is exhausted.
                let export = match inner.lock() {
                    Ok(mut exporter) => {
                        // A tenant group retains this resource across retries,
                        // even when another group changes the shared exporter.
                        exporter.set_resource(&resource);
                        exporter.export(batch.clone())
                    }
                    Err(error) => {
                        let error = TraceError::from(error.to_string());
                        return Err(log_drop(&batch, attempts, error, "exporter lock failed"));
                    }
                };
                let result = match timeout(remaining.min(policy.attempt_timeout), export).await {
                    Ok(result) => result,
                    Err(_) => Err(TraceError::ExportTimedOut(
                        remaining.min(policy.attempt_timeout),
                    )),
                };
                match result {
                    Ok(()) => return Ok(()),
                    Err(error) if is_transient(&error) => {
                        let remaining = policy.max_elapsed.saturating_sub(started.elapsed());
                        if remaining.is_zero() {
                            return Err(log_drop(
                                &batch,
                                attempts,
                                error,
                                "retry window exhausted",
                            ));
                        }
                        tracing::warn!(
                            attempts,
                            span_count = batch.len(),
                            retry_in_ms = backoff.min(remaining).as_millis() as u64,
                            error = %error,
                            "OTLP span export failed; retrying batch"
                        );
                        sleep(backoff.min(remaining)).await;
                        backoff = backoff.saturating_mul(2).min(policy.max_backoff);
                    }
                    Err(error) => {
                        return Err(log_drop(
                            &batch,
                            attempts,
                            error,
                            "non-retryable export failure",
                        ));
                    }
                }
            }
        })
    }

    fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Ok(mut exporter) = self.inner.lock() {
            exporter.shutdown();
        }
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.resource = resource.clone();
    }
}

fn is_transient(error: &TraceError) -> bool {
    match error {
        TraceError::ExportTimedOut(_) => true,
        TraceError::ExportFailed(error) => {
            let source: &(dyn Error + 'static) = error.as_ref();
            matches!(
                source.downcast_ref::<OtlpError>(),
                Some(OtlpError::Status {
                    code: Code::Unavailable
                        | Code::DeadlineExceeded
                        | Code::ResourceExhausted
                        | Code::Aborted
                        | Code::Internal
                        | Code::Unknown,
                    ..
                })
            )
        }
        _ => false,
    }
}

fn log_drop(batch: &[SpanData], attempts: usize, error: TraceError, reason: &str) -> TraceError {
    tracing::warn!(
        dropped_spans = batch.len(),
        attempts,
        reason,
        error = %error,
        "OTLP span batch permanently dropped"
    );
    TraceError::from(OtlpBatchError(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    use opentelemetry::trace::{
        SpanContext, SpanId, SpanKind, Status, TraceFlags, TraceId, TraceState,
    };
    use opentelemetry::InstrumentationScope;
    use opentelemetry_otlp::{WithExportConfig, WithTonicConfig};
    use opentelemetry_proto::tonic::collector::trace::v1::{
        trace_service_server::{TraceService, TraceServiceServer},
        ExportTraceServiceRequest, ExportTraceServiceResponse,
    };
    use opentelemetry_sdk::runtime;
    use opentelemetry_sdk::trace::{
        BatchConfigBuilder, BatchSpanProcessor, SpanEvents, SpanLinks, SpanProcessor,
    };
    use std::time::SystemTime;
    use tonic::transport::{Channel, Server};

    fn span() -> SpanData {
        SpanData {
            span_context: SpanContext::new(
                TraceId::from_u128(1),
                SpanId::from_u64(2),
                TraceFlags::SAMPLED,
                false,
                TraceState::default(),
            ),
            parent_span_id: SpanId::INVALID,
            span_kind: SpanKind::Internal,
            name: "retry-test".into(),
            start_time: SystemTime::now(),
            end_time: SystemTime::now(),
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            events: SpanEvents::default(),
            links: SpanLinks::default(),
            status: Status::Unset,
            instrumentation_scope: InstrumentationScope::builder("retry-test").build(),
        }
    }

    fn short_policy() -> RetryPolicy {
        RetryPolicy {
            max_elapsed: Duration::from_millis(75),
            initial_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(10),
            attempt_timeout: Duration::from_millis(20),
        }
    }

    #[derive(Debug)]
    struct FailingExporter {
        attempts: Arc<Mutex<Vec<Vec<SpanData>>>>,
        failures_left: usize,
        shutdown_called: Arc<AtomicBool>,
    }

    impl SpanExporter for FailingExporter {
        fn export(&mut self, batch: Vec<SpanData>) -> BoxFuture<'static, ExportResult> {
            self.attempts.lock().unwrap().push(batch);
            let result = if self.failures_left > 0 {
                self.failures_left -= 1;
                Err(TraceError::from(OtlpError::Status {
                    code: Code::Unavailable,
                    message: "collector unavailable".into(),
                }))
            } else {
                Ok(())
            };
            Box::pin(async move { result })
        }

        fn shutdown(&mut self) {
            self.shutdown_called.store(true, Ordering::Release);
        }
    }

    fn failing_exporter(
        failures_left: usize,
    ) -> (
        FailingExporter,
        Arc<Mutex<Vec<Vec<SpanData>>>>,
        Arc<AtomicBool>,
    ) {
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let shutdown_called = Arc::new(AtomicBool::new(false));
        (
            FailingExporter {
                attempts: attempts.clone(),
                failures_left,
                shutdown_called: shutdown_called.clone(),
            },
            attempts,
            shutdown_called,
        )
    }

    #[derive(Clone, Debug)]
    struct Collector {
        received: Arc<AtomicUsize>,
    }

    #[derive(Debug)]
    struct ObservingExporter {
        inner: opentelemetry_otlp::SpanExporter,
        failed_attempts: Arc<AtomicUsize>,
    }

    impl SpanExporter for ObservingExporter {
        fn export(&mut self, batch: Vec<SpanData>) -> BoxFuture<'static, ExportResult> {
            let export = self.inner.export(batch);
            let failed_attempts = self.failed_attempts.clone();
            Box::pin(async move {
                let result = export.await;
                if result.is_err() {
                    failed_attempts.fetch_add(1, Ordering::Relaxed);
                }
                result
            })
        }

        fn shutdown(&mut self) {
            self.inner.shutdown();
        }

        fn set_resource(&mut self, resource: &Resource) {
            self.inner.set_resource(resource);
        }
    }

    #[tonic::async_trait]
    impl TraceService for Collector {
        async fn export(
            &self,
            request: tonic::Request<ExportTraceServiceRequest>,
        ) -> Result<tonic::Response<ExportTraceServiceResponse>, tonic::Status> {
            let count = request
                .into_inner()
                .resource_spans
                .iter()
                .flat_map(|resource| &resource.scope_spans)
                .map(|scope| scope.spans.len())
                .sum::<usize>();
            self.received.fetch_add(count, Ordering::Relaxed);
            Ok(tonic::Response::new(ExportTraceServiceResponse::default()))
        }
    }

    #[tokio::test]
    async fn refused_collector_connection_recovers_without_losing_batch() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let channel = Channel::from_shared(format!("http://{address}"))
            .unwrap()
            .timeout(Duration::from_millis(100))
            .connect_lazy();
        let otlp = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_timeout(Duration::from_millis(100))
            .with_channel(channel)
            .build()
            .unwrap();
        let failed_attempts = Arc::new(AtomicUsize::new(0));
        let mut retrying = RetryingSpanExporter::with_policy(
            ObservingExporter {
                inner: otlp,
                failed_attempts: failed_attempts.clone(),
            },
            RetryPolicy {
                max_elapsed: Duration::from_secs(2),
                initial_backoff: Duration::from_millis(20),
                max_backoff: Duration::from_millis(50),
                attempt_timeout: Duration::from_millis(100),
            },
        );
        retrying.set_resource(&Resource::empty());
        let batch = vec![span()];
        let export = retrying.export(batch);
        let export_task = tokio::spawn(export);

        // Let the first connection attempt reach the closed port before the
        // collector comes online, as happens during a node rollout.
        sleep(Duration::from_millis(150)).await;
        let received = Arc::new(AtomicUsize::new(0));
        let collector = Collector {
            received: received.clone(),
        };
        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(TraceServiceServer::new(collector))
                .serve(address)
                .await
        });

        timeout(Duration::from_secs(3), export_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(received.load(Ordering::Relaxed), 1);
        assert!(failed_attempts.load(Ordering::Relaxed) >= 1);
        retrying.shutdown();
        server.abort();
    }

    #[tokio::test]
    async fn retry_exhaustion_returns_error_and_retries_the_same_batch() {
        let (exporter, attempts, _) = failing_exporter(usize::MAX);
        let mut retrying = RetryingSpanExporter::with_policy(exporter, short_policy());
        let original = vec![span()];
        let error = retrying.export(original.clone()).await.unwrap_err();
        assert!(format!("{error:?}").contains(OTLP_EXPORT_ERROR_MARKER));
        assert!(error.to_string().contains(OTLP_EXPORT_ERROR_MARKER));
        let attempts = attempts.lock().unwrap();
        assert!(attempts.len() > 1);
        assert!(attempts.iter().all(|batch| batch == &original));
    }

    #[tokio::test]
    async fn shutdown_flushes_a_batch_through_a_transient_failure() {
        let (exporter, attempts, shutdown_called) = failing_exporter(1);
        let processor = BatchSpanProcessor::builder(
            RetryingSpanExporter::with_policy(exporter, short_policy()),
            runtime::Tokio,
        )
        .with_batch_config(
            BatchConfigBuilder::default()
                .with_max_queue_size(4)
                .with_max_export_batch_size(4)
                .with_scheduled_delay(Duration::from_secs(60))
                .with_max_export_timeout(Duration::from_secs(1))
                .build(),
        )
        .build();
        processor.on_end(span());
        let result = tokio::task::spawn_blocking(move || processor.shutdown())
            .await
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(attempts.lock().unwrap().len(), 2);
        assert!(shutdown_called.load(Ordering::Acquire));
    }
}
