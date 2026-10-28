use opentelemetry::{
    baggage::BaggageExt,
    global,
    propagation::TextMapCompositePropagator,
    trace::{TraceResult, TracerProvider as _},
    Context, KeyValue,
};
use opentelemetry_otlp::{WithExportConfig, WithTonicConfig};
use opentelemetry_sdk::{
    export::trace::SpanData,
    propagation::{BaggagePropagator, TraceContextPropagator},
    runtime,
    trace::{
        BatchConfig, BatchConfigBuilder, BatchSpanProcessor, RandomIdGenerator, Sampler, Span,
        SpanProcessor, TracerProvider,
    },
    Resource,
};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tonic::transport::{Channel, ClientTlsConfig};
use tracing_subscriber::{
    layer::SubscriberExt, registry::LookupSpan, util::SubscriberInitExt, EnvFilter, Layer,
};

pub mod braintrust;
mod datadog_json;
mod otlp_retry;
pub mod sampling;
pub mod tenant_export;
use crate::config::{LogFormat, RouterConfig};
use crate::sampling_policy::MatchContext;
use datadog_json::{DatadogJsonFormatter, PrettyLogFormatter};
use sampling::{
    FrontendRouteSampler, RequestSampling, SamplingContextSpanProcessor, SamplingGate,
    SamplingGateSpanProcessor,
};

/// Runtime handles for settings that can be changed without restarting the
/// router. Spans are stamped with the active Braintrust generation when queued;
/// batches from older generations are discarded after a destination update.
#[derive(Clone)]
pub struct TelemetryRuntime {
    braintrust: Arc<braintrust::BraintrustConfigState>,
    /// Shared state of the trace-sampling gate installed in front of the OTLP
    /// exporter. Held here so the policy store, which does not exist yet when
    /// telemetry is initialized, can be attached later.
    sampling: Arc<SamplingGate>,
}

impl TelemetryRuntime {
    pub fn register_environment(
        &self,
        environment: &str,
        runtime: &Self,
        store: crate::storage::SharedObjectStore,
        deployment: &str,
    ) {
        self.braintrust.register_environment(
            environment,
            runtime.braintrust.clone(),
            store,
            telemetry_resource(environment, deployment),
        );
    }

    pub fn new(initial: Vec<braintrust::BraintrustConfig>) -> Self {
        Self {
            braintrust: Arc::new(braintrust::BraintrustConfigState::new(initial)),
            sampling: Arc::new(SamplingGate::new()),
        }
    }

    /// Mark this environment's telemetry destinations as tenant-chosen, so they
    /// may only be public addresses (see [`crate::egress`]).
    pub fn set_public_egress_only(&self, public_egress_only: bool) {
        self.braintrust.set_public_egress_only(public_egress_only);
    }

    pub fn public_egress_only(&self) -> bool {
        self.braintrust.public_egress_only()
    }

    pub fn braintrust_state(&self) -> Arc<braintrust::BraintrustConfigState> {
        self.braintrust.clone()
    }

    /// Give the sampling gate the environment's policy store. Until this is
    /// called the gate has no policy to read and keeps every span; see
    /// [`SamplingGate`] for why the store cannot be supplied at
    /// [`init_telemetry`] time.
    pub fn set_sampling_policy_store(
        &self,
        store: Arc<dyn crate::sampling_policy::SamplingPolicyStore>,
    ) {
        self.sampling.set_store(store);
    }

    /// Give the sampling gate the HMAC key it hashes `session.id`/trace ids
    /// with, so the keep/drop mapping is not predictable to whoever supplies
    /// those values. See [`sampling::SamplingGate::set_sampling_secret`].
    pub fn set_sampling_secret(&self, secret: Vec<u8>) {
        self.sampling.set_sampling_secret(secret);
    }

    /// Load the sampling policy once, so the first requests after boot are
    /// decided by it instead of keeping every trace until the first
    /// span-triggered refresh lands.
    pub async fn prime_sampling_policy(&self) {
        self.sampling.refresh().await;
    }

    /// The gate [`OtelMakeSpan`] resolves each request's keep decision against.
    /// The same handle the export-side processor holds, so a force-keep found
    /// mid-request reaches the spans that already carry a decision.
    pub fn sampling_gate(&self) -> Arc<SamplingGate> {
        self.sampling.clone()
    }

    pub fn set_braintrust_configs(&self, configs: Vec<braintrust::BraintrustConfig>) {
        let Some(previous) = self.braintrust.replace(configs.clone()) else {
            return;
        };

        for removed in previous.iter().filter(|config| !configs.contains(config)) {
            tracing::info!(
                endpoint = %removed.traces_url,
                parent = %removed.parent,
                "Stopped exporting frontend spans to Braintrust destination"
            );
        }
        for added in configs.iter().filter(|config| !previous.contains(config)) {
            tracing::info!(
                endpoint = %added.traces_url,
                parent = %added.parent,
                "Started exporting frontend spans to Braintrust destination"
            );
        }
    }
}

/// Effective endpoint of the process-wide OTLP/gRPC pipeline.
pub fn otlp_endpoint_from_env() -> String {
    std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://localhost:4317".to_string())
}

/// Span attribute Chalk resolves an agent session from, and the baggage key callers put it
/// under. Chalk's ClickHouse `SessionId` column coalesces six spellings; this is the first,
/// and the one chalkcompute's `enter_session` sets.
pub const SESSION_ID_ATTRIBUTE: &str = "session.id";

/// Resource attribute Chalk uses to scope traces to an environment.
pub const ENVIRONMENT_ID_ATTRIBUTE: &str = "chalk.environment_id";

/// OpenInference's span-kind attribute (`LLM`, `EMBEDDING`, `TOOL`, ...). Every
/// LLM-instrumented span in the router carries it; it is also how the Braintrust
/// exporter recognizes a span worth forwarding.
pub const OPENINFERENCE_SPAN_KIND_ATTRIBUTE: &str = "openinference.span.kind";

/// Span attribute naming the inbound API dialect a request arrived on
/// (`openai` / `anthropic`). Stamped on the per-request span of an LLM route.
pub const FRONTEND_ATTRIBUTE: &str = "chalk.frontend";

/// Span attribute naming the specific frontend operation (`chat.completions`,
/// `messages`, `embeddings`, ...).
pub const FRONTEND_ENDPOINT_ATTRIBUTE: &str = "chalk.frontend.endpoint";

/// Span attribute naming the caller the request authenticated as.
///
/// The value is the key prefix of the resolved API key (`ck-1a2b3c4d`) — the
/// non-secret display half of the credential, which identifies the key without
/// being enough to use it. A request authenticated by a federated Chalk JWT
/// instead carries `user:<sub>`: the normalized `Claims` the auth crate hands
/// back keep `sub` and drop everything else, so there is no client or
/// environment id to prefer over it (and the environment is already a resource
/// attribute on every span this process exports).
pub const CALLER_IDENTITY_ATTRIBUTE: &str = "chalk.caller.identity";

/// Span attribute carrying the caller-supplied request tag — a freeform label
/// naming the call site or purpose of the call (`openai_complete`), which the
/// sampling policy can assign its own keep rate.
pub const CALLER_REQUEST_TAG_ATTRIBUTE: &str = "chalk.request_tag";

/// Span attribute that pins a span for export regardless of the sampling rate.
///
/// Set at request time by the force-trace debug header, stamped on the request
/// span when the response turns out to be an error (see [`error_force_keep`]),
/// and settable by a handler through
/// [`crate::handlers::trace_attributes::LlmSpanWriter`] for outcome-based keeps
/// — a moderation check that flags a request, say.
pub const SAMPLING_FORCE_KEEP_ATTRIBUTE: &str = "chalk.sampling.force_keep";

/// Span attribute carrying a response-content-derived keep rate that overrides
/// the request-time rate-based decision for this trace, set by a handler
/// through [`crate::handlers::trace_attributes::LlmSpanWriter`] when a
/// [`crate::sampling_policy::OutcomeRule`] matches the response body. Unlike
/// [`SAMPLING_FORCE_KEEP_ATTRIBUTE`] this can resolve to either keep or drop —
/// see [`crate::telemetry::sampling::SamplingGate::resolve_outcome`].
pub const SAMPLING_OUTCOME_RATE_ATTRIBUTE: &str = "chalk.sampling.outcome_rate";

/// Baggage key carrying whether this request's content may be recorded, as the
/// matched sampling rule decided.
///
/// Baggage only, never a span attribute: it is read by handler code as it
/// writes a span (see [`crate::handlers::content_logging_enabled`]), and the
/// export path has no use for it. Reserved like the others so a caller cannot
/// turn content logging back on for a key an operator silenced.
pub const SAMPLING_LOG_CONTENT_ATTRIBUTE: &str = "chalk.sampling.log_content";

/// Span attribute carrying the rate-based keep decision for a whole trace.
///
/// Resolved once, at request entry, by [`OtelMakeSpan`], and stamped on every
/// span of the request from baggage — so the gate reads one decision off
/// whichever span it is looking at instead of re-deriving it per span, and a
/// span with no LLM markers of its own cannot escape its trace's decision.
///
/// Present only on the spans of a recognized LLM frontend route. Its absence is
/// meaningful: it is what leaves every other route governed by
/// `OTEL_TRACES_SAMPLER` alone.
///
/// Internal to the gate. [`sampling::SamplingGateSpanProcessor`] strips it
/// before export, because only kept spans are exported and the value is
/// therefore always `true` on the wire.
pub const SAMPLING_DECISION_ATTRIBUTE: &str = "chalk.sampling.decision";

/// Header naming the call site or purpose of a request, for
/// [`CALLER_REQUEST_TAG_ATTRIBUTE`]. Freeform: any call path may set one, and
/// a tag the policy does not mention simply falls back to its default rate.
pub const REQUEST_TAG_HEADER: &str = "x-chalk-request-tag";

/// Header that forces a request's trace to be exported, for debugging.
pub const FORCE_TRACE_HEADER: &str = "x-chalk-force-trace";

/// Longest accepted `x-chalk-request-tag`. Tags are attacker-supplied and end
/// up as span attributes and policy-map lookups, so bound them at the door.
const MAX_REQUEST_TAG_BYTES: usize = 256;

/// OpenTelemetry's default per-span attribute limit. Resolve the standard
/// environment override once and use the same value for the SDK and the
/// handler-side priority allocator so they cannot silently drift apart.
static SPAN_ATTRIBUTE_COUNT_LIMIT: OnceLock<usize> = OnceLock::new();
static TRACE_BATCH_LIMITS: OnceLock<(usize, usize)> = OnceLock::new();

const DEFAULT_BSP_MAX_QUEUE_SIZE: usize = 2_048;
const DEFAULT_BSP_MAX_EXPORT_BATCH_SIZE: usize = 512;
const MAX_SAFE_EXPORT_BATCH_SIZE: usize = 16;

pub fn span_attribute_count_limit() -> usize {
    *SPAN_ATTRIBUTE_COUNT_LIMIT.get_or_init(|| {
        std::env::var("OTEL_SPAN_ATTRIBUTE_COUNT_LIMIT")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(128)
    })
}

fn parse_positive_usize_env(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn capped_trace_batch_limits(
    requested_queue_size: usize,
    requested_batch_size: usize,
) -> (usize, usize) {
    // Queue capacity is a throughput setting. A count-derived cgroup ceiling
    // based on the maximum possible LLM span size made the default queue only
    // 60 entries in a 512 MiB pod even though normal spans are a few KiB, and
    // silently overrode an operator's explicit OTEL_BSP_MAX_QUEUE_SIZE. Keep
    // request/body bounds at admission sites and leave queue sizing to the
    // standard OpenTelemetry setting. A true memory ceiling requires a
    // byte-accounted queue rather than a pessimistic span-count heuristic.
    let queue_size = requested_queue_size.max(1);
    let batch_size = requested_batch_size
        .min(MAX_SAFE_EXPORT_BATCH_SIZE)
        .min(queue_size)
        .max(1);
    (queue_size, batch_size)
}

fn trace_batch_limits() -> (usize, usize) {
    *TRACE_BATCH_LIMITS.get_or_init(|| {
        capped_trace_batch_limits(
            parse_positive_usize_env("OTEL_BSP_MAX_QUEUE_SIZE", DEFAULT_BSP_MAX_QUEUE_SIZE),
            parse_positive_usize_env(
                "OTEL_BSP_MAX_EXPORT_BATCH_SIZE",
                DEFAULT_BSP_MAX_EXPORT_BATCH_SIZE,
            ),
        )
    })
}

pub(crate) fn trace_batch_config() -> BatchConfig {
    let (queue_size, batch_size) = trace_batch_limits();
    BatchConfigBuilder::default()
        .with_max_queue_size(queue_size)
        .with_max_export_batch_size(batch_size)
        // One in-flight batch keeps the retry backlog inside the queue cap.
        .with_max_concurrent_exports(1)
        // The SDK otherwise cancels an in-flight export after 30 seconds,
        // before the retry wrapper can bridge a collector rollout.
        .with_max_export_timeout(otlp_retry::PROCESSOR_TIMEOUT)
        .build()
}

/// One of the router's inbound LLM API surfaces — a "frontend".
///
/// The router speaks several public dialects across a handful of operations,
/// and fans each out to whichever upstream the
/// model resolves to. Which dialect a request arrived on is not recoverable from
/// the LLM span alone: `llm.system` describes the *upstream* provider, so an
/// `anthropic/...` model called through `/v1/chat/completions` and the same model
/// called through `/v1/messages` produce identical LLM attributes. Recording the
/// frontend on the request span keeps the two distinguishable — in Chalk's
/// traces and, once forwarded, as Braintrust metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frontend {
    /// The API dialect: `openai`, `anthropic`, or `gemini`.
    pub api: &'static str,
    /// The operation within that dialect.
    pub endpoint: &'static str,
}

impl Frontend {
    const fn new(api: &'static str, endpoint: &'static str) -> Self {
        Self { api, endpoint }
    }
}

/// Classify a request path as one of the router's LLM frontends, or `None` for
/// everything else (admin, gRPC, health, model listing, span ingest).
///
/// Most routes are matched exactly: `/v1/admin/...` and `/v1/models` share the
/// `/v1/` root, and `/v1/traces/spans` is an ingest
/// endpoint for spans a client already produced, not an LLM call of our own
/// (those spans reach Braintrust on their own OpenInference merits).
pub fn frontend_for_path(path: &str) -> Option<Frontend> {
    Some(match path {
        "/v1/chat/completions" => Frontend::new("openai", "chat.completions"),
        "/v1/alpha/decisions" => Frontend::new("chalk", "decisions"),
        "/v1/decisions" => Frontend::new("openai", "decisions"),
        "/v1/systemone" => Frontend::new("typesafe", "systemone"),
        "/v1/responses" => Frontend::new("openai", "responses"),
        "/v1/responses/compact" => Frontend::new("openai", "responses.compact"),
        path if path.starts_with("/v1/responses/") => Frontend::new("openai", "responses.item"),
        "/v1beta/interactions" => Frontend::new("gemini", "interactions"),
        "/v1/messages" => Frontend::new("anthropic", "messages"),
        "/v1/messages/count_tokens" => Frontend::new("anthropic", "messages.count_tokens"),
        "/v1/embeddings" => Frontend::new("openai", "embeddings"),
        "/v1/images/generations" => Frontend::new("openai", "images.generations"),
        "/v1/images/edits" => Frontend::new("openai", "images.edits"),
        "/v1/images/variations" => Frontend::new("openai", "images.variations"),
        "/v1/audio/speech" => Frontend::new("openai", "audio.speech"),
        "/v1/audio/transcriptions" => Frontend::new("openai", "audio.transcriptions"),
        "/v1/audio/translations" => Frontend::new("openai", "audio.translations"),
        "/v1/moderations" => Frontend::new("openai", "moderations"),
        _ => return None,
    })
}

/// The propagators the router registers globally: W3C Trace Context and Baggage.
///
/// Without a registered text-map propagator the global `extract`/`inject` calls are no-ops,
/// so inbound `traceparent` headers would be ignored and downstream calls would not carry
/// context. Baggage is registered beside it because that is where a caller puts the session
/// id — a header is the only per-request channel, and the completion APIs have no field for
/// it (nor should they: one conversation spans many requests).
///
/// Exposed so tests exercise the same propagators production registers.
pub fn text_map_propagator() -> TextMapCompositePropagator {
    TextMapCompositePropagator::new(vec![
        Box::new(TraceContextPropagator::new()),
        Box::new(BaggagePropagator::new()),
    ])
}

/// Copies the caller's session id from baggage onto every span the router starts.
///
/// A session is a property of the caller's conversation, so it is never a field on a
/// completion request: the same conversation spans many of those. It travels beside the
/// request instead, and baggage is the single form this processor reads —
/// [`with_agent_session_id`] normalizes the other spellings into baggage at request entry so
/// there is one place a session can come from. Chalk groups a *trace* by the `session.id`
/// attribute of its spans, so the id has to be on the spans themselves before export, not
/// merely in the context.
///
/// A span processor rather than a stamp at each span site: it sees every span the process
/// starts, including the ones `tracing` macros create deep in a handler and any a future
/// instrumentation layer adds, so a new span cannot silently leave its session behind.
#[derive(Debug, Default)]
pub struct SessionIdSpanProcessor;

impl SpanProcessor for SessionIdSpanProcessor {
    fn on_start(&self, span: &mut Span, cx: &Context) {
        // `cx` is the span's parent context, not necessarily the current one, so read the
        // baggage from it rather than from `Context::current()`.
        if let Some(session_id) = cx.baggage().get(SESSION_ID_ATTRIBUTE) {
            use opentelemetry::trace::Span as _;
            span.set_attribute(KeyValue::new(SESSION_ID_ATTRIBUTE, session_id.clone()));
        }
    }

    fn on_end(&self, _span: SpanData) {}

    fn force_flush(&self) -> TraceResult<()> {
        Ok(())
    }

    fn shutdown(&self) -> TraceResult<()> {
        Ok(())
    }
}

/// Initialize OpenTelemetry with OTLP gRPC exporter
pub fn init_telemetry(
    object_store: crate::storage::SharedObjectStore,
    config: &RouterConfig,
) -> Result<TelemetryRuntime, Box<dyn std::error::Error>> {
    // Get OTLP endpoint from environment variable, default to localhost:4317
    let otlp_endpoint = otlp_endpoint_from_env();

    tracing::info!(
        "Initializing OpenTelemetry with endpoint: {}",
        otlp_endpoint
    );

    let use_tls = otlp_endpoint.starts_with("https://");

    // Build the gRPC channel ourselves so we can enable HTTP/2 keepalive. Without
    // it, the long-lived connection to a load-balanced collector (e.g. the Google
    // Frontend in front of telemetry.chalk.ai) is closed after a few minutes, and
    // the tonic exporter never reconnects, so spans silently stop being exported.
    // Keepalive pings keep the connection warm and let tonic detect a dead
    // connection and reconnect.
    let mut endpoint = Channel::from_shared(otlp_endpoint.clone())?
        .timeout(otlp_retry::RPC_TIMEOUT)
        .http2_keep_alive_interval(Duration::from_secs(30))
        .keep_alive_timeout(Duration::from_secs(10))
        .keep_alive_while_idle(true)
        .tcp_keepalive(Some(Duration::from_secs(30)));

    if use_tls {
        endpoint = endpoint.tls_config(ClientTlsConfig::new().with_native_roots())?;
    }

    let channel = endpoint.connect_lazy();

    let otlp_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_timeout(otlp_retry::RPC_TIMEOUT)
        .with_channel(channel)
        .build()?;

    // Singular routers retain deployment-wide attribution. Cluster routers use
    // trusted per-span attribution to group OTLP resources during export.
    let resource = telemetry_resource(
        if config.cluster.is_some() {
            ""
        } else {
            &config.environment_id
        },
        &config.deployment_env,
    );

    // Braintrust destinations and the sampling policy are both refreshed at
    // runtime, so the runtime handle is built before the provider that uses it.
    let braintrust_configs: Vec<_> = braintrust::BraintrustConfig::from_env()
        .into_iter()
        .collect();
    let runtime_handle = TelemetryRuntime::new(braintrust_configs.clone());

    // Configure tracer provider with resource attributes.
    //
    // LLM-frontend spans are always recorded and gated at export instead
    // (`sampling::SamplingGateSpanProcessor`), because the rate depends on
    // things — a request tag, the caller's key — that a sampler cannot see, and
    // a flagged outcome is not known until the request has been served. Every
    // other route keeps the `OTEL_TRACES_SAMPLER` behavior, both in the sampler
    // and past the gate.
    // An `OTEL_TRACES_SAMPLER` that switches tracing off keeps its meaning:
    // frontend routes are left to it rather than force-recorded.
    let env_sampler = sampler_from_env();
    let sampler = FrontendRouteSampler::new(env_sampler.sampler, !env_sampler.disables_tracing);
    let otlp_processor = SamplingGateSpanProcessor::new(
        BatchSpanProcessor::builder(
            tenant_export::TenantResourceExporter::new(otlp_exporter),
            runtime::Tokio,
        )
        .with_batch_config(trace_batch_config())
        .build(),
        runtime_handle.sampling.clone(),
    );
    let mut provider_builder = TracerProvider::builder()
        .with_span_processor(sampling::TenantAttributionSpanProcessor)
        .with_span_processor(otlp_processor)
        // Before the batch exporter sees the span at on_end, so the attribute is exported.
        .with_span_processor(SessionIdSpanProcessor)
        // Likewise: the gate reads the keep decision off the span at on_end,
        // and it is stamped from the request's baggage here at on_start.
        .with_span_processor(SamplingContextSpanProcessor)
        .with_sampler(sampler)
        .with_id_generator(RandomIdGenerator::default())
        .with_max_attributes_per_span(
            u32::try_from(span_attribute_count_limit()).unwrap_or(u32::MAX),
        )
        .with_resource(resource.clone());

    // Braintrust destinations are dynamically refreshed from the settings
    // store. The processor is installed once so an exporter added at runtime
    // can take effect without replacing the global provider; while the
    // snapshot is empty, it skips queueing spans and its worker stays idle.
    //
    // Gated by the same `SamplingGate` instance as the OTLP path, so both
    // destinations reach the identical keep/drop answer for a span — including a
    // force-keep, which pins the trace in the shared gate rather than per
    // exporter. The two filters stack rather than overlap: this gate decides
    // whether a span is exported at all, and the Braintrust processor still
    // decides which of the kept spans Braintrust itself wants.
    //
    // INVARIANT for any exporter added here later: wrap it in
    // `SamplingGateSpanProcessor` with this same gate. A processor registered
    // bare exports every recorded span at full volume whatever the sampling
    // policy says — LLM-frontend spans are always *recorded* and thinned only at
    // export, so an unwrapped destination sees all of them. `telemetry_settings`
    // is built for more exporter kinds than Braintrust, and each one has to be
    // wrapped as it is wired in.
    let runtime = runtime_handle;
    let braintrust_processor = SamplingGateSpanProcessor::new(
        braintrust::span_processor(runtime.braintrust_state(), &resource, object_store),
        runtime.sampling.clone(),
    );
    provider_builder = provider_builder.with_span_processor(braintrust_processor);

    let tracer_provider = provider_builder.build();

    // Set the global tracer provider
    global::set_tracer_provider(tracer_provider.clone());

    global::set_text_map_propagator(text_map_propagator());

    // Create OpenTelemetry tracing layer.
    // Always suppress h2 spans — they are high-volume and low-value.
    let otel_filter = {
        let base = tracing_subscriber::EnvFilter::try_from_env("OTEL_LOG_LEVEL")
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("trace"));
        base.add_directive("h2=off".parse().unwrap())
    };
    let telemetry_layer = tracing_opentelemetry::layer()
        .with_tracer(tracer_provider.tracer("chalk-router"))
        .with_filter(otel_filter);

    // Create the stdout logging layer. `LOG_FORMAT` selects the formatter:
    // Datadog-compatible JSON (the default) or a human-readable format for local dev.
    let fmt_layer = fmt_layer(config.log_format, config.dd_env.clone());

    // Initialize tracing subscriber with both layers
    tracing_subscriber::registry()
        .with(telemetry_layer)
        .with(fmt_layer)
        .init();

    let (queue_size, batch_size) = trace_batch_limits();
    tracing::info!(queue_size, batch_size, "Configured trace export batches");

    // Only now is there a subscriber to log to — anything emitted above this
    // line is dropped. Whether a deployment is shipping prompt content to a
    // third party is exactly the kind of thing an operator must be able to
    // confirm from the boot log, so report it here rather than at the decision.
    if braintrust_configs.is_empty() {
        tracing::debug!("Braintrust span export disabled (no enabled exporters)");
    } else {
        for config in &braintrust_configs {
            tracing::info!(
            endpoint = %config.traces_url,
            parent = %config.parent,
            "Exporting frontend spans to Braintrust"
            );
        }
    }

    Ok(runtime)
}

fn telemetry_resource(environment_id: &str, deployment_env: &str) -> Resource {
    let mut attributes = vec![
        KeyValue::new("service.name", "chalk-router"),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
        KeyValue::new("deployment.environment", deployment_env.to_string()),
    ];
    if !environment_id.is_empty() {
        attributes.push(KeyValue::new(
            ENVIRONMENT_ID_ATTRIBUTE,
            environment_id.to_string(),
        ));
    }
    Resource::new(attributes)
}

/// Build the stdout logging layer, with its own env filter (defaulting to a
/// global `info` floor so we always emit at least info-level logs) and the
/// given formatter (`log_format`, `dd_env` — see [`RouterConfig::log_format`]
/// and [`RouterConfig::dd_env`]). Boxed so both arms share one type.
fn fmt_layer<S>(log_format: LogFormat, dd_env: Option<String>) -> Box<dyn Layer<S> + Send + Sync>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    // `rmcp` isn't a dependency here (it's the gateway's MCP crate); the directive
    // is inert today and kept for parity with chalk-mcp-gateway's default.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,rmcp=warn"));

    match log_format {
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .event_format(DatadogJsonFormatter::new("chalk-router", dd_env))
            .with_filter(filter)
            .boxed(),
        LogFormat::Pretty => tracing_subscriber::fmt::layer()
            .event_format(PrettyLogFormatter)
            .with_filter(filter)
            .boxed(),
    }
}

/// Install only the stdout logging layer, without OTLP export or Braintrust.
///
/// For one-shot subcommands such as `migrate` that exit before the serve
/// stack exists (and before a [`RouterConfig`] is resolved). Without a
/// subscriber their `tracing` events (lease waits, heartbeat failures,
/// adopted history rows) are silently dropped, so the operator reading the
/// Job's logs would never see them. Reads `LOG_FORMAT`/`DD_ENV` directly
/// (matching `RouterConfig::from_env`'s parsing) since no config exists yet
/// on this path.
pub fn init_logging_only() {
    tracing_subscriber::registry()
        .with(fmt_layer(
            LogFormat::from_env(),
            std::env::var("DD_ENV").ok().filter(|v| !v.is_empty()),
        ))
        .init();
}

/// Select the trace sampler from the environment, following the OpenTelemetry
/// `OTEL_TRACES_SAMPLER` / `OTEL_TRACES_SAMPLER_ARG` convention.
///
/// `OTEL_TRACES_SAMPLER` accepts: `always_on` (default), `always_off`,
/// `traceidratio`, `parentbased_always_on`, `parentbased_always_off`,
/// `parentbased_traceidratio`. For the ratio-based samplers, the sampling
/// probability is read from `OTEL_TRACES_SAMPLER_ARG` (a float in `[0.0, 1.0]`,
/// defaulting to `1.0`). Unknown values fall back to `always_on`.
fn sampler_from_env() -> EnvSampler {
    let kind = std::env::var("OTEL_TRACES_SAMPLER").unwrap_or_else(|_| "always_on".to_string());

    let ratio = std::env::var("OTEL_TRACES_SAMPLER_ARG")
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map(|r| r.clamp(0.0, 1.0))
        .unwrap_or(1.0);

    let normalized = kind.trim().to_ascii_lowercase();
    let sampler = match normalized.as_str() {
        "always_on" => Sampler::AlwaysOn,
        "always_off" => Sampler::AlwaysOff,
        "traceidratio" => Sampler::TraceIdRatioBased(ratio),
        "parentbased_always_on" => Sampler::ParentBased(Box::new(Sampler::AlwaysOn)),
        "parentbased_always_off" => Sampler::ParentBased(Box::new(Sampler::AlwaysOff)),
        "parentbased_traceidratio" => {
            Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(ratio)))
        }
        other => {
            tracing::warn!(
                sampler = %other,
                "Unknown OTEL_TRACES_SAMPLER value; falling back to always_on"
            );
            Sampler::AlwaysOn
        }
    };

    tracing::info!("Trace sampler: {} (ratio: {})", kind, ratio);
    EnvSampler {
        disables_tracing: tracing_switched_off(&normalized),
        sampler,
    }
}

/// The environment's sampler, plus whether it switches tracing off outright.
struct EnvSampler {
    sampler: Sampler,
    disables_tracing: bool,
}

/// Whether an `OTEL_TRACES_SAMPLER` value means "do not trace".
///
/// Such a deployment has opted out, and the LLM-frontend policy must not
/// resurrect it: `FrontendRouteSampler` leaves the environment's sampler in
/// charge of frontend routes too when this is true, so the off switch keeps
/// working regardless of what the sampling policy says.
fn tracing_switched_off(normalized_kind: &str) -> bool {
    matches!(normalized_kind, "always_off" | "parentbased_always_off")
}

/// Shutdown OpenTelemetry gracefully, flushing remaining spans
pub fn shutdown_telemetry() {
    global::shutdown_tracer_provider();
}

/// Build an OpenTelemetry context from an inbound request's W3C trace headers,
/// using the globally-registered text-map propagator.
///
/// We can't hand the raw `traceparent` straight to `opentelemetry-rust`: its W3C
/// `TraceContextPropagator` (through at least 0.31) rejects any version-0
/// `traceparent` whose trace-flags field is greater than `2` — the `opts > 2`
/// check in `opentelemetry_sdk`'s `trace_context.rs`. Modern OpenTelemetry SDKs
/// (Python, JS, ...) set the W3C Trace Context **Level 2** "random" flag
/// (`0x02`) alongside "sampled" (`0x01`), so a normal sampled request arrives
/// with flags `03`. The strict parser silently drops it, discarding the inbound
/// trace. We normalize the flags down to just the sampled bit (all the parser
/// keeps anyway, see its `& TraceFlags::SAMPLED`) so the header is accepted and
/// the trace is continued. trace_id / span_id / tracestate are left untouched.
fn extract_parent_context(headers: &axum::http::HeaderMap) -> opentelemetry::Context {
    let mut carrier: HashMap<String, String> = HashMap::new();
    if let Some(traceparent) = headers.get("traceparent").and_then(|v| v.to_str().ok()) {
        carrier.insert(
            "traceparent".to_string(),
            normalize_traceparent(traceparent),
        );
    }
    if let Some(tracestate) = headers.get("tracestate").and_then(|v| v.to_str().ok()) {
        carrier.insert("tracestate".to_string(), tracestate.to_string());
    }
    // Baggage rides beside the trace context and carries the session id. It is independent
    // of `traceparent`: a caller may send one without the other, and dropping it silently
    // was why a router span could never be attributed to a conversation.
    if let Some(baggage) = headers
        .get("baggage")
        .and_then(|v| v.to_str().ok())
        .filter(|value| value.len() <= 8192)
    {
        carrier.insert("baggage".to_string(), baggage.to_string());
    }
    let cx = global::get_text_map_propagator(|propagator| propagator.extract(&carrier));
    let baggage: Vec<_> = cx
        .baggage()
        .iter()
        .take(64)
        .filter(|(key, (value, _))| {
            !key.as_str().starts_with("chalk.")
                && !matches!(
                    key.as_str(),
                    "environment_id" | "team_id" | "service.name" | "deployment.environment"
                )
                && key.as_str().len() <= 128
                && value.as_str().len() <= 1024
                && !value.as_str().chars().any(char::is_control)
        })
        .map(|(key, (value, _))| KeyValue::new(key.clone(), value.clone()))
        .collect();
    let cx = cx.with_cleared_baggage().with_baggage(baggage);
    with_agent_session_id(&cx, headers)
}

/// Chalk's own header for naming an agent session. A caller that sets it has decided what its
/// sessions are, so it outranks every other rule below, baggage included.
pub const AGENT_SESSION_ID_HEADER: &str = "x-chalk-agent-session-id";

/// Session-id header spellings coding-agent harnesses set on their own, tried in order and only
/// once the two Chalk-controlled sources are exhausted.
///
/// Each entry is that harness's *conversation* id — one value for every turn of one session, which
/// is what makes it a session id at all. Per-request and per-window ids that sit right beside them
/// on the wire (`x-client-request-id`, `x-codex-window-id`) are deliberately absent: adopting one
/// would put every turn in a session of its own, which is the state this list exists to fix.
const HARNESS_SESSION_ID_HEADERS: &[&str] = &[
    // Claude Code. The same uuid is also duplicated into the body's `metadata.user_id` JSON, but
    // the Anthropic handler drops `metadata` as client content, so the header is the copy we see.
    "x-claude-code-session-id",
    // Codex CLI. It sends `session-id` and `thread-id` with the same uuid today; `thread-id` is
    // second so that if they ever diverge the conversation wins over the process that ran it.
    "session-id",
    "thread-id",
];

/// Longest header value accepted as a session id. The ids these harnesses send are uuids, and
/// chalkcompute's are short hashes, so this only has to be generous enough to leave room for a
/// prefixed scheme. Its job is to keep an id an id: the value lands in baggage and then on the
/// `session.id` attribute of every span of the request, and `SessionId` is an indexed ClickHouse
/// column, so a caller must not be able to stream content into it through a header.
const MAX_SESSION_ID_LEN: usize = 200;

/// Resolves the agent session for a request and returns `cx` carrying it in baggage, which is
/// where [`SessionIdSpanProcessor`] looks when it stamps `session.id` on every span the request
/// starts.
///
/// Precedence, highest first:
///
/// 1. [`AGENT_SESSION_ID_HEADER`], Chalk's own spelling. Explicit beats inferred, and it is the
///    one a caller can set without also owning its OpenTelemetry configuration.
/// 2. Baggage, the W3C-standard route, already extracted into `cx` by the propagators. This is
///    what chalkcompute's `enter_session` sets, and honoring it above the harness headers keeps a
///    Chalk-instrumented caller that happens to run inside a coding agent grouped by its own
///    session rather than by the agent's.
/// 3. [`HARNESS_SESSION_ID_HEADERS`], for the harnesses that ship no baggage at all. Claude Code
///    and Codex both send a session id on every request and neither sends `baggage`, so without
///    this rule their traffic arrives as one orphan trace per turn.
fn with_agent_session_id(cx: &Context, headers: &axum::http::HeaderMap) -> Context {
    if let Some(session_id) = session_id_header(headers, AGENT_SESSION_ID_HEADER) {
        // `with_baggage` merges, so this overwrites an inbound baggage session id rather than
        // losing to it -- rule 1 outranks rule 2.
        return cx.with_baggage(vec![KeyValue::new(SESSION_ID_ATTRIBUTE, session_id)]);
    }
    if cx.baggage().get(SESSION_ID_ATTRIBUTE).is_some() {
        return cx.clone();
    }
    match HARNESS_SESSION_ID_HEADERS
        .iter()
        .find_map(|name| session_id_header(headers, name))
    {
        Some(session_id) => cx.with_baggage(vec![KeyValue::new(SESSION_ID_ATTRIBUTE, session_id)]),
        None => cx.clone(),
    }
}

/// Reads one header as a session id: present, ASCII, non-empty once trimmed, and no longer than
/// [`MAX_SESSION_ID_LEN`]. A value failing any of those is treated as absent so the next rule
/// still gets its turn -- a malformed id from one harness must not mask a good one beside it, and
/// no request is ever rejected over telemetry.
fn session_id_header(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    let value = headers.get(name)?.to_str().ok()?.trim();
    if value.is_empty() || value.len() > MAX_SESSION_ID_LEN {
        return None;
    }
    Some(value.to_string())
}

/// Mask a W3C version-0 `traceparent`'s trace-flags down to the `sampled` bit so
/// the strict `opentelemetry-rust` parser accepts flags like `03`
/// (`sampled | random`). Returns the input unchanged if it is not a well-formed
/// version-0 header (other versions aren't subject to the `opts > 2` check).
fn normalize_traceparent(traceparent: &str) -> String {
    let parts: Vec<&str> = traceparent.split('-').collect();
    if parts.len() == 4 && parts[0] == "00" {
        if let Ok(flags) = u8::from_str_radix(parts[3], 16) {
            return format!("00-{}-{}-{:02x}", parts[1], parts[2], flags & 0x01);
        }
    }
    traceparent.to_string()
}

/// `MakeSpan` implementation for `tower_http`'s `TraceLayer` that extracts the
/// inbound trace context from request headers and sets it as the parent of the
/// per-request span. This makes chalk-router continue a caller's distributed
/// trace (honoring `traceparent`) instead of starting a fresh root span.
///
/// It also resolves the request-scoped sampling inputs — who is calling, the
/// tag they gave the call, their key's own rate, and the force-trace escape
/// hatch — and, for an LLM frontend route, the rate-based keep decision itself.
/// Two of those inputs come from `req.extensions()`, which means
/// `auth_middleware` has to have run first; the layer order in `main` puts it
/// outside the `TraceLayer` for exactly this reason. Everything that feeds the
/// keep decision, and the decision, go into the context's baggage rather than
/// onto this span alone, so every span of the request is decided the same way
/// (see [`sampling::with_sampling_inputs`]).
///
/// Request entry is the earliest point where the session or trace id, the
/// request tag and the resolved per-key rate are all in hand at once, and
/// deciding here is what lets a span with no LLM markers of its own — a
/// `policy_cache_refresh` opened by [`crate::fallback`] or
/// [`crate::judge_policy`] while the request is being served — be dropped with
/// the rest of its trace.
#[derive(Clone, Debug)]
pub struct OtelMakeSpan {
    sampling: Arc<SamplingGate>,
}

impl OtelMakeSpan {
    pub fn new(sampling: Arc<SamplingGate>) -> Self {
        Self { sampling }
    }

    /// What the policy says about a frontend request, or `None` when there is no
    /// usable key to decide on.
    ///
    /// `parent_cx` must already be this span's parent: the trace id is the
    /// caller's when the request continued a trace, and reading it before the
    /// parent is attached would key the decision on an id no exported span
    /// carries.
    ///
    /// An invalid trace id means nothing is tracing this span (no OpenTelemetry
    /// layer installed, or the span was filtered away), so nothing is stamped
    /// and the trace falls through to the gate's forward-everything branch.
    /// Without that, every such request would share one all-zero key.
    fn decide_for_request(
        &self,
        span: &tracing::Span,
        parent_cx: &Context,
        ctx: &MatchContext,
    ) -> Option<RequestSampling> {
        use opentelemetry::trace::TraceContextExt;
        use tracing_opentelemetry::OpenTelemetrySpanExt;

        // The same baggage `SessionIdSpanProcessor` stamps from, rather than a
        // second reading of the header: a session id makes the whole
        // conversation one decision, and the two must not be able to disagree.
        let session_id = parent_cx
            .baggage()
            .get(SESSION_ID_ATTRIBUTE)
            .map(|session_id| session_id.as_str().to_string());
        let key = match &session_id {
            Some(session_id) => session_id.clone(),
            None => {
                let trace_id = span.context().span().span_context().trace_id();
                if trace_id == opentelemetry::trace::TraceId::INVALID {
                    return None;
                }
                format!("{trace_id}")
            }
        };
        Some(self.sampling.decide(&key, ctx, session_id.is_some()))
    }
}

impl<B> tower_http::trace::MakeSpan<B> for OtelMakeSpan {
    fn make_span(&mut self, req: &axum::http::Request<B>) -> tracing::Span {
        use tracing_opentelemetry::OpenTelemetrySpanExt;

        let path = req.uri().path();
        if matches!(path, "/health" | "/v1/healthcheck")
            || path.starts_with("/grpc.health.v1.Health/")
        {
            // Probes never create traces, even with an inbound sampled parent
            // or a force-trace header. Skip span creation rather than sampling.
            return tracing::Span::none();
        }

        let span = tracing::info_span!(
            "http_request",
            otel.name = %format!("{} {}", req.method(), req.uri().path()),
            http.method = %req.method(),
            http.target = %req.uri(),
        );
        let identity = caller_identity(req.extensions());
        let match_context = MatchContext {
            request_tag: request_tag(req.headers()),
            // The rule condition is the same string a span carries as
            // `chalk.caller.identity`, so a rule can be written from what an
            // operator reads in a trace.
            key_id: identity.clone(),
            usage_pool_id: caller_usage_pool(req.extensions()),
        };
        let force_keep = force_trace_requested(req.headers());
        let tenant = req.extensions().get::<crate::tenancy::RequestRuntime>();
        let gate = tenant
            .map(|tenant| tenant.0.sampling_gate.clone())
            .unwrap_or_else(|| self.sampling.clone());
        let selector = Self::new(gate.clone());
        let mut parent_cx = sampling::with_sampling_inputs(
            extract_parent_context(req.headers()),
            match_context.request_tag.clone(),
            force_keep,
            None,
        );
        if let Some(tenant) = tenant {
            parent_cx = parent_cx.with_value(sampling::TenantSampling {
                environment_id: tenant.0.environment_id.clone(),
                gate: gate.clone(),
            });
        }
        span.set_parent(parent_cx.clone());
        // An LLM route's request span is the root of what an observability
        // backend shows as one call, so name the frontend on it. This is also
        // the marker the Braintrust exporter filters on, which is why it is set
        // here — on every LLM route at once — rather than in each handler.
        if let Some(frontend) = frontend_for_path(req.uri().path()) {
            span.set_attribute(FRONTEND_ATTRIBUTE, frontend.api);
            span.set_attribute(FRONTEND_ENDPOINT_ATTRIBUTE, frontend.endpoint);
            // Frontend routes only: resolving the decision pre-samples the span
            // to learn its trace id, and every other route must stay governed
            // by `OTEL_TRACES_SAMPLER` alone.
            if let Some(resolved) = selector.decide_for_request(&span, &parent_cx, &match_context) {
                // Re-parent so the decision reaches every span of the trace as
                // baggage. Re-parenting only replaces the parent context and
                // clears the pre-sample, leaving the parent linkage and the
                // decision taken when the span closes unchanged.
                use opentelemetry::trace::TraceContextExt;
                let trace_id = span.context().span().span_context().trace_id();
                let parent_cx = gate.with_trace_decision(parent_cx, trace_id);
                span.set_parent(sampling::with_sampling_inputs(
                    parent_cx,
                    match_context.request_tag,
                    force_keep,
                    Some(resolved),
                ));
            }
        }
        // Identity stays on the request span only: it names who made the call,
        // a fact about the request rather than something the export path reads.
        if let Some(identity) = identity {
            span.set_attribute(CALLER_IDENTITY_ATTRIBUTE, identity);
        }
        span
    }
}

/// Pin a request's trace for export when its response is an error.
///
/// A failure is the trace an operator came looking for, so it is kept whatever
/// rate matched. This is the `TraceLayer` response hook rather than a check
/// inside the handlers, because the signal is the status actually returned to
/// the caller: an attempt that failed and was recovered by a fallback returns
/// 200 and is a success for this purpose.
///
/// Both halves of the existing force-keep mechanism are used. The attribute
/// keeps the request span, which is still open here; the gate's trace-id set
/// keeps every span of the trace that has yet to end. Spans that already ended
/// and were dropped cannot be recovered — a trace whose rate dropped it and
/// whose error surfaced only in the response is exported from the request span
/// down, not whole.
fn error_force_keep(
    gate: &Arc<SamplingGate>,
    status: axum::http::StatusCode,
    span: &tracing::Span,
) {
    use opentelemetry::trace::TraceContextExt;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    if !status.is_client_error() && !status.is_server_error() {
        return;
    }
    span.set_attribute(SAMPLING_FORCE_KEEP_ATTRIBUTE, true);
    let context = span.context();
    let gate = context
        .get::<sampling::TenantSampling>()
        .map(|tenant| &tenant.gate)
        .unwrap_or(gate);
    let trace_id = span.context().span().span_context().trace_id();
    if trace_id != opentelemetry::trace::TraceId::INVALID {
        gate.force_keep_trace(trace_id);
    }
}

/// `TraceLayer`'s response hook: keeps an errored request's trace, then defers
/// to the default hook so the response is still logged as before.
#[derive(Clone, Debug)]
pub struct OtelOnResponse {
    sampling: Arc<SamplingGate>,
    inner: tower_http::trace::DefaultOnResponse,
}

impl OtelOnResponse {
    pub fn new(sampling: Arc<SamplingGate>) -> Self {
        Self {
            sampling,
            inner: tower_http::trace::DefaultOnResponse::new(),
        }
    }
}

impl<B> tower_http::trace::OnResponse<B> for OtelOnResponse {
    fn on_response(
        self,
        response: &axum::http::Response<B>,
        latency: Duration,
        span: &tracing::Span,
    ) {
        error_force_keep(&self.sampling, response.status(), span);
        self.inner.on_response(response, latency, span);
    }
}

/// The caller a request authenticated as, for [`CALLER_IDENTITY_ATTRIBUTE`].
///
/// Both shapes `auth_middleware` inserts are accepted. The API key is checked
/// first because it is what the LLM routes carry: the proxied surface turns a
/// federated JWT into a synthetic key too, and only the admin surface leaves
/// bare `Claims` in the extensions.
fn caller_identity(extensions: &axum::http::Extensions) -> Option<String> {
    if let Some(key) = extensions.get::<Arc<crate::api_keys::ApiKey>>() {
        return Some(key.key_prefix.clone());
    }
    extensions
        .get::<Arc<chalk_federated_auth::Claims>>()
        .map(|claims| format!("user:{}", claims.sub))
}

/// The usage pool the resolved key belongs to, when it is in one. Matched
/// against a sampling rule's `usage_pool_id`.
fn caller_usage_pool(extensions: &axum::http::Extensions) -> Option<String> {
    extensions
        .get::<Arc<crate::api_keys::ApiKey>>()
        .and_then(|key| key.usage_pool_id.clone())
}

/// The caller's request tag, bounded and required to be non-empty. A tag that
/// is empty or oversized is dropped rather than truncated: a truncated tag
/// would silently match a different policy entry than the caller asked for.
fn request_tag(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(REQUEST_TAG_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|tag| !tag.is_empty() && tag.len() <= MAX_REQUEST_TAG_BYTES)
        .map(str::to_string)
}

/// Whether the force-trace debug header asks for this request to be kept.
/// Any truthy value counts; an explicit `0`/`false` does not.
fn force_trace_requested(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(FORCE_TRACE_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            let value = value.trim();
            !value.is_empty()
                && !value.eq_ignore_ascii_case("0")
                && !value.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::propagation::TextMapPropagator;
    use opentelemetry::trace::TraceContextExt;

    const TID: &str = "0af7651916cd43dd8448eb211c80319c";
    const SID: &str = "b7ad6b7169203331";

    #[test]
    fn health_checks_never_export_request_spans() {
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
        use opentelemetry_sdk::trace::{SimpleSpanProcessor, TracerProvider};
        use tower_http::trace::MakeSpan;
        use tracing_subscriber::layer::SubscriberExt;

        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("health-test")));
        let gate = Arc::new(SamplingGate::new());
        let mut make_span = OtelMakeSpan::new(gate.clone());

        tracing::subscriber::with_default(subscriber, || {
            for path in [
                "/health",
                "/health?probe=ready",
                "/v1/healthcheck",
                "/grpc.health.v1.Health/Check",
                "/grpc.health.v1.Health/Watch",
            ] {
                let req = axum::http::Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("traceparent", format!("00-{TID}-{SID}-01"))
                    .header(FORCE_TRACE_HEADER, "true")
                    .body(())
                    .unwrap();
                let span = make_span.make_span(&req);
                assert!(span.is_none(), "{path} must not create a span");
                // An unhealthy probe must not revive tracing either.
                error_force_keep(&gate, axum::http::StatusCode::SERVICE_UNAVAILABLE, &span);
            }
            assert!(exporter.get_finished_spans().unwrap().is_empty());

            for path in ["/v1/models", "/chalk.router.v1.ApiKeyService/CreateKey"] {
                let req = axum::http::Request::builder().uri(path).body(()).unwrap();
                let span = make_span.make_span(&req);
                assert!(!span.is_none(), "{path} must still create a span");
            }
        });
        assert_eq!(exporter.get_finished_spans().unwrap().len(), 2);
    }

    fn braintrust_config(parent: &str) -> braintrust::BraintrustConfig {
        braintrust::BraintrustConfig {
            traces_url: "https://api.braintrust.dev/otel/v1/traces".to_string(),
            api_key: format!("secret-{parent}"),
            parent: parent.to_string(),
        }
    }

    #[test]
    fn destination_changes_advance_the_generation() {
        let old = braintrust_config("project_name:old");
        let new = braintrust_config("project_name:new");
        let runtime = TelemetryRuntime::new(vec![old.clone()]);
        let state = runtime.braintrust_state();
        assert_eq!(state.snapshot(), (0, vec![old]));

        runtime.set_braintrust_configs(vec![new.clone()]);

        assert_eq!(state.snapshot(), (1, vec![new]));
    }

    #[test]
    fn runtime_updates_recover_a_poisoned_config_lock() {
        let runtime = TelemetryRuntime::new(Vec::new());
        let poison = runtime.braintrust_state();
        assert!(std::thread::spawn(move || poison.poison_configs())
            .join()
            .is_err());
        let config = braintrust::BraintrustConfig {
            traces_url: "https://api.braintrust.dev/otel/v1/traces".to_string(),
            api_key: "secret".to_string(),
            parent: "project_name:test".to_string(),
        };

        runtime.set_braintrust_configs(vec![config.clone()]);

        assert_eq!(runtime.braintrust_state().snapshot().1, vec![config]);
    }

    #[test]
    fn telemetry_resource_includes_nonempty_environment_id() {
        let resource = telemetry_resource("env-123", "local");

        assert_eq!(
            resource
                .get(opentelemetry::Key::new(ENVIRONMENT_ID_ATTRIBUTE))
                .map(|value| value.to_string()),
            Some("env-123".to_string())
        );
    }

    #[test]
    fn telemetry_resource_omits_unset_or_empty_environment_id() {
        // `RouterConfig::environment_id` collapses both "unset" and
        // "explicitly empty" to "" before this function ever sees it (see
        // `env_nonempty`), so there is only one empty-input case to check here.
        let resource = telemetry_resource("", "local");

        assert!(resource
            .get(opentelemetry::Key::new(ENVIRONMENT_ID_ATTRIBUTE))
            .is_none());
    }

    #[test]
    fn batch_queue_honors_the_operator_while_export_batches_are_capped() {
        assert_eq!(
            capped_trace_batch_limits(2_048, 512),
            (2_048, MAX_SAFE_EXPORT_BATCH_SIZE)
        );
        assert_eq!(capped_trace_batch_limits(8, 4), (8, 4));
        assert_eq!(capped_trace_batch_limits(1, 512), (1, 1));
    }

    #[test]
    fn classifies_both_api_dialects_as_frontends() {
        assert_eq!(
            frontend_for_path("/v1/chat/completions"),
            Some(Frontend::new("openai", "chat.completions"))
        );
        assert_eq!(
            frontend_for_path("/v1/alpha/decisions"),
            Some(Frontend::new("chalk", "decisions"))
        );
        assert_eq!(
            frontend_for_path("/v1/decisions"),
            Some(Frontend::new("openai", "decisions"))
        );
        assert_eq!(
            frontend_for_path("/v1/systemone"),
            Some(Frontend::new("typesafe", "systemone"))
        );
        assert_eq!(
            frontend_for_path("/v1/responses"),
            Some(Frontend::new("openai", "responses"))
        );
        assert_eq!(
            frontend_for_path("/v1/responses/compact"),
            Some(Frontend::new("openai", "responses.compact"))
        );
        assert_eq!(
            frontend_for_path("/v1/messages"),
            Some(Frontend::new("anthropic", "messages"))
        );
        assert_eq!(
            frontend_for_path("/v1/messages/count_tokens"),
            Some(Frontend::new("anthropic", "messages.count_tokens"))
        );
        // Non-chat modalities are frontends too — they are LLM calls.
        assert_eq!(
            frontend_for_path("/v1/embeddings").map(|f| f.endpoint),
            Some("embeddings")
        );
        assert_eq!(
            frontend_for_path("/v1/audio/transcriptions").map(|f| f.api),
            Some("openai")
        );
    }

    #[test]
    fn management_and_ingest_paths_are_not_frontends() {
        for path in [
            "/v1/admin/keys",
            "/v1/models",
            "/v1/models/gpt-4o",
            "/v1/debug/completions",
            "/health",
            "/chalk.router.v1.ApiKeyService/CreateKey",
            // Spans a client already produced; they reach an observability
            // backend on their own OpenInference attributes, and the POST that
            // carried them is not an LLM call.
            "/v1/traces/spans",
            // Prefix-matching would misclassify these.
            "/v1/chat/completions/extra",
            "/v1/messages/count_tokens/extra",
        ] {
            assert_eq!(
                frontend_for_path(path),
                None,
                "{path} should not be a frontend"
            );
        }
    }

    fn resolved_session_id(headers: &[(&str, &str)]) -> Option<String> {
        global::set_text_map_propagator(text_map_propagator());
        let mut map = axum::http::HeaderMap::new();
        for (name, value) in headers {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(value).unwrap(),
            );
        }
        extract_parent_context(&map)
            .baggage()
            .get(SESSION_ID_ATTRIBUTE)
            .map(|value| value.as_str().to_string())
    }

    #[test]
    fn baggage_limits_and_reserved_tenant_controls_are_enforced() {
        global::set_text_map_propagator(text_map_propagator());
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("baggage", "session.id=ok,chalk.environment_id=other,chalk.sampling.force_keep=true,team_id=other,custom=value".parse().unwrap());
        let context = extract_parent_context(&headers);
        assert_eq!(context.baggage().get("session.id").unwrap().as_str(), "ok");
        assert_eq!(context.baggage().get("custom").unwrap().as_str(), "value");
        assert!(context.baggage().get("chalk.environment_id").is_none());
        assert!(context.baggage().get("chalk.sampling.force_keep").is_none());
        assert!(context.baggage().get("team_id").is_none());
        headers.insert(
            "baggage",
            format!("session.id={}", "x".repeat(8193)).parse().unwrap(),
        );
        assert!(extract_parent_context(&headers).baggage().is_empty());
    }

    #[test]
    fn chalk_header_outranks_baggage_and_harness_headers() {
        assert_eq!(
            resolved_session_id(&[
                ("x-chalk-agent-session-id", "chalk-1"),
                ("baggage", "session.id=bag-1"),
                ("x-claude-code-session-id", "cc-1"),
                ("session-id", "codex-1"),
            ]),
            Some("chalk-1".to_string())
        );
    }

    #[test]
    fn baggage_outranks_harness_headers() {
        // A chalkcompute caller running inside a coding agent stays grouped by its own session.
        assert_eq!(
            resolved_session_id(&[
                ("baggage", "session.id=cs-1"),
                ("x-claude-code-session-id", "cc-1"),
            ]),
            Some("cs-1".to_string())
        );
    }

    #[test]
    fn harness_headers_apply_in_order_when_nothing_else_is_set() {
        assert_eq!(
            resolved_session_id(&[("x-claude-code-session-id", "cc-1"), ("session-id", "cx-1")]),
            Some("cc-1".to_string())
        );
        assert_eq!(
            resolved_session_id(&[("session-id", "cx-1"), ("thread-id", "th-1")]),
            Some("cx-1".to_string())
        );
        assert_eq!(
            resolved_session_id(&[("thread-id", "th-1")]),
            Some("th-1".to_string())
        );
    }

    #[test]
    fn a_request_with_no_session_anywhere_gets_none() {
        assert_eq!(resolved_session_id(&[]), None);
        assert_eq!(
            resolved_session_id(&[("traceparent", &format!("00-{TID}-{SID}-01"))]),
            None
        );
    }

    #[test]
    fn a_session_id_survives_alongside_a_continued_trace() {
        // The two are independent: honoring one must not drop the other.
        global::set_text_map_propagator(text_map_propagator());
        let mut map = axum::http::HeaderMap::new();
        map.insert(
            "traceparent",
            axum::http::HeaderValue::from_str(&format!("00-{TID}-{SID}-03")).unwrap(),
        );
        map.insert(
            "x-claude-code-session-id",
            axum::http::HeaderValue::from_static("cc-1"),
        );
        let cx = extract_parent_context(&map);
        assert_eq!(
            cx.baggage()
                .get(SESSION_ID_ATTRIBUTE)
                .map(|value| value.as_str().to_string()),
            Some("cc-1".to_string())
        );
        assert_eq!(format!("{}", cx.span().span_context().trace_id()), TID);
    }

    #[test]
    fn an_unusable_value_falls_through_to_the_next_rule() {
        // Empty and whitespace-only ids would group unrelated callers under one blank session.
        assert_eq!(
            resolved_session_id(&[
                ("x-chalk-agent-session-id", "   "),
                ("x-claude-code-session-id", "cc-1"),
            ]),
            Some("cc-1".to_string())
        );
        let too_long = "x".repeat(MAX_SESSION_ID_LEN + 1);
        assert_eq!(
            resolved_session_id(&[
                ("x-claude-code-session-id", &too_long),
                ("session-id", "cx-1"),
            ]),
            Some("cx-1".to_string())
        );
        assert_eq!(
            resolved_session_id(&[("x-chalk-agent-session-id", &too_long)]),
            None
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_off_an_id() {
        assert_eq!(
            resolved_session_id(&[("x-chalk-agent-session-id", "  chalk-1  ")]),
            Some("chalk-1".to_string())
        );
    }

    #[test]
    fn real_harness_session_ids_are_accepted_verbatim() {
        // Captured on the wire from claude-cli 2.1.263 and codex-cli 0.153.4.
        assert_eq!(
            resolved_session_id(&[(
                "x-claude-code-session-id",
                "307c9672-086e-4c40-a7a1-191e3c86dfe4"
            )]),
            Some("307c9672-086e-4c40-a7a1-191e3c86dfe4".to_string())
        );
        assert_eq!(
            resolved_session_id(&[
                ("session-id", "01a079c9-a39e-7530-9cce-8c09fc9dcd3a"),
                ("thread-id", "01a079c9-a39e-7530-9cce-8c09fc9dcd3a"),
                (
                    "x-client-request-id",
                    "01a079c9-a39e-7530-9cce-8c09fc9dcd3a"
                ),
                (
                    "x-codex-window-id",
                    "01a079c9-a39e-7530-9cce-8c09fc9dcd3a:0"
                ),
                ("originator", "codex_exec"),
            ]),
            Some("01a079c9-a39e-7530-9cce-8c09fc9dcd3a".to_string())
        );
    }

    fn headers(entries: &[(&str, &str)]) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        for (name, value) in entries {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn request_tag_is_trimmed_bounded_and_optional() {
        assert_eq!(
            request_tag(&headers(&[(REQUEST_TAG_HEADER, "  openai_complete ")])).as_deref(),
            Some("openai_complete")
        );
        assert_eq!(request_tag(&headers(&[])), None);
        assert_eq!(request_tag(&headers(&[(REQUEST_TAG_HEADER, "   ")])), None);
        let oversized = "t".repeat(MAX_REQUEST_TAG_BYTES + 1);
        assert_eq!(
            request_tag(&headers(&[(REQUEST_TAG_HEADER, &oversized)])),
            None,
            "an oversized tag is dropped, never truncated into a different tag"
        );
    }

    #[test]
    fn force_trace_accepts_any_truthy_value() {
        for value in ["1", "true", "TRUE", "yes", "debug-me"] {
            assert!(
                force_trace_requested(&headers(&[(FORCE_TRACE_HEADER, value)])),
                "{value} should force a trace"
            );
        }
        for value in ["0", "false", "False", "", "  "] {
            assert!(
                !force_trace_requested(&headers(&[(FORCE_TRACE_HEADER, value)])),
                "{value} should not force a trace"
            );
        }
        assert!(!force_trace_requested(&headers(&[])));
    }

    #[test]
    fn caller_identity_prefers_the_resolved_api_key() {
        let mut extensions = axum::http::Extensions::new();
        assert_eq!(caller_identity(&extensions), None);
        assert_eq!(caller_usage_pool(&extensions), None);

        extensions.insert(Arc::new(chalk_federated_auth::Claims {
            sub: "user-7".to_string(),
            exp: 0,
            scopes: Vec::new(),
        }));
        assert_eq!(caller_identity(&extensions).as_deref(), Some("user:user-7"));

        extensions.insert(Arc::new(crate::api_keys::ApiKey {
            key_hash: "hash".to_string(),
            key_prefix: "ck-1a2b3c4d".to_string(),
            description: String::new(),
            created_at: chrono::Utc::now(),
            permission: crate::api_keys::ApiKeyPermission::default(),
            labels: HashMap::new(),
            usage_pool_id: Some("pool-internal".to_string()),
            daily_token_budget: None,
        }));
        assert_eq!(
            caller_identity(&extensions).as_deref(),
            Some("ck-1a2b3c4d"),
            "the LLM routes always carry a key, synthetic or stored"
        );
        // Both are rule conditions, and both come off the same resolved key.
        assert_eq!(
            caller_usage_pool(&extensions).as_deref(),
            Some("pool-internal")
        );
    }

    #[test]
    fn normalize_masks_level2_random_flag() {
        // sampled | random (modern SDKs) -> sampled
        assert_eq!(
            normalize_traceparent(&format!("00-{TID}-{SID}-03")),
            format!("00-{TID}-{SID}-01")
        );
        // sampled-only is unchanged
        assert_eq!(
            normalize_traceparent(&format!("00-{TID}-{SID}-01")),
            format!("00-{TID}-{SID}-01")
        );
        // not-sampled | random -> not sampled
        assert_eq!(
            normalize_traceparent(&format!("00-{TID}-{SID}-02")),
            format!("00-{TID}-{SID}-00")
        );
    }

    #[test]
    fn normalize_passes_through_malformed_or_other_versions() {
        assert_eq!(normalize_traceparent("garbage"), "garbage");
        // version != 0 is not subject to the opts>2 check, so leave it alone
        assert_eq!(
            normalize_traceparent(&format!("01-{TID}-{SID}-09")),
            format!("01-{TID}-{SID}-09")
        );
    }

    #[test]
    fn raw_flags_03_is_rejected_by_upstream_parser() {
        // Guards the assumption motivating the normalization: opentelemetry-rust
        // drops a flags=03 header outright.
        let mut carrier = HashMap::new();
        carrier.insert("traceparent".to_string(), format!("00-{TID}-{SID}-03"));
        let cx = TraceContextPropagator::new().extract(&carrier);
        assert!(!cx.span().span_context().is_valid());
    }

    #[test]
    fn normalized_flags_03_is_parsed_and_continues_trace() {
        let mut carrier = HashMap::new();
        carrier.insert(
            "traceparent".to_string(),
            normalize_traceparent(&format!("00-{TID}-{SID}-03")),
        );
        let cx = TraceContextPropagator::new().extract(&carrier);
        let sc = cx.span().span_context().clone();
        assert!(
            sc.is_valid(),
            "expected a valid span context after normalization"
        );
        assert!(sc.is_remote());
        assert!(sc.is_sampled());
        assert_eq!(format!("{}", sc.trace_id()), TID);
        assert_eq!(format!("{}", sc.span_id()), SID);
    }
}
