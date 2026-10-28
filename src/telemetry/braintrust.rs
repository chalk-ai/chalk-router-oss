//! Braintrust span export for the router's inbound LLM frontends.
//!
//! The router already produces OpenInference-conformant LLM spans (see
//! `AGENTS.md`) and ships every span it starts to Chalk's OTLP collector. This
//! module adds a *second*, independent destination: Braintrust, whose OTLP
//! ingest endpoint understands the OpenInference semantic conventions natively
//! — `openinference.span.kind = "LLM"` becomes a Braintrust `llm` span,
//! `llm.input_messages.*` / `llm.output_messages.*` become its `input` /
//! `output`, and `llm.token_count.*` becomes its `metrics`. Nothing about the
//! spans themselves has to change for Braintrust to read them.
//!
//! Two things do have to be arranged, and they are what this module is:
//!
//! 1. **A separate exporter.** Braintrust needs its own endpoint, a bearer
//!    token, and an `x-bt-parent` header naming the project spans land in.
//!    `opentelemetry-otlp`'s HTTP builder cannot be used for it: its
//!    `resolve_http_endpoint` prefers `OTEL_EXPORTER_OTLP_ENDPOINT` over the
//!    endpoint the caller passes, and the router sets that variable to Chalk's
//!    collector in every deployment — so a builder-constructed Braintrust
//!    exporter would quietly POST Braintrust-authenticated payloads at Chalk's
//!    collector instead. [`BraintrustSpanExporter`] encodes the same OTLP
//!    protobuf body and posts it with `reqwest`, taking its endpoint from
//!    config only.
//!
//! 2. **A filter.** Braintrust is an LLM-observability product; the router's
//!    admin RPCs, health probes, cache refreshes, and Postgres spans are noise
//!    there (and billable volume). [`BraintrustSpanProcessor`] forwards only
//!    *frontend* spans: the per-request span of an inbound LLM API route (see
//!    [`crate::telemetry::Frontend`]) and any span carrying an
//!    `openinference.span.kind` — which covers the handlers' own LLM/EMBEDDING
//!    spans and the agent-side spans replayed through `/v1/traces/spans`.
//!    Everything else still goes to Chalk's collector, unchanged.
//!
//! Parent links are forwarded exactly as they are. A trace whose intermediate
//! spans were filtered out (or whose root lives in the caller's own tracing
//! backend) reaches Braintrust with a dangling parent id; Braintrust accepts
//! those and still groups the spans, because it derives a trace's identity from
//! the OTLP trace id rather than from reaching a root span.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, OnceLock, RwLock,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{
    future::{join_all, BoxFuture},
    stream, StreamExt,
};
use opentelemetry::trace::{TraceError, TraceResult};
use opentelemetry::Context;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, InstrumentationScope, KeyValue};
use opentelemetry_proto::tonic::resource::v1::Resource as ProtoResource;
use opentelemetry_proto::tonic::trace::v1::{span, ResourceSpans, ScopeSpans, Span as ProtoSpan};
use opentelemetry_proto::transform::common::tonic::ResourceAttributesWithSchema;
use opentelemetry_proto::transform::trace::tonic::group_spans_by_resource_and_scope;
use opentelemetry_sdk::export::trace::{ExportResult, SpanData, SpanExporter};
use opentelemetry_sdk::trace::{BatchSpanProcessor, Span, SpanProcessor};
use opentelemetry_sdk::{runtime, Resource};
use prost::Message;

use super::{FRONTEND_ATTRIBUTE, OPENINFERENCE_SPAN_KIND_ATTRIBUTE};
use crate::storage::SharedObjectStore;

/// Braintrust's hosted API. Overridable for self-hosted / proxied installs.
const DEFAULT_API_URL: &str = "https://api.braintrust.dev";

/// Path Braintrust serves OTLP/HTTP trace ingest on, appended to the API URL.
const OTEL_TRACES_PATH: &str = "/otel/v1/traces";

/// Project spans land in when none is configured. Named after the service so a
/// misconfigured deployment is obvious in the Braintrust project list rather
/// than silently merging into someone else's project.
const DEFAULT_PROJECT: &str = "chalk-router";

/// Upper bound on one export POST. Longer than a healthy round-trip by a wide
/// margin; short enough that a wedged Braintrust cannot stall the batch
/// processor's export slot indefinitely (it exports serially).
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);
/// Hydration is best-effort and must leave most of the export window for the
/// Braintrust POST. On timeout the original bounded spans are still exported.
const HYDRATION_TIMEOUT: Duration = Duration::from_secs(3);

/// How much of an error response body is quoted back in a log line.
const ERROR_BODY_LIMIT: usize = 512;
/// Preserve the pre-bounding behavior for Braintrust without putting larger
/// objects back into the router's shared span buffers.
const MAX_HYDRATED_CONTENT_BYTES: usize = 256 * 1024;
const MAX_CONCURRENT_HYDRATIONS: usize = 2 * super::MAX_SAFE_EXPORT_BATCH_SIZE;

/// Internal marker binding a queued span to one destination generation.
const CONFIG_GENERATION_ATTRIBUTE: &str = "chalk.braintrust.config_generation";

#[derive(Debug)]
struct BraintrustConfigSnapshot {
    generation: i64,
    configs: Vec<BraintrustConfig>,
}

/// Shared destination snapshot. The generation and configs deliberately live
/// under one lock so an export can never pair values from different updates.
#[derive(Debug)]
pub struct BraintrustConfigState {
    snapshot: RwLock<BraintrustConfigSnapshot>,
    tenants: RwLock<std::collections::BTreeMap<String, BraintrustTenant>>,
    /// Exports for this environment may reach public addresses only (see
    /// [`crate::egress`]): its destinations are chosen by a tenant.
    public_egress_only: AtomicBool,
}

#[derive(Clone)]
struct BraintrustTenant {
    state: Arc<BraintrustConfigState>,
    resource: Resource,
    store: SharedObjectStore,
}

impl std::fmt::Debug for BraintrustTenant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BraintrustTenant").finish_non_exhaustive()
    }
}

impl BraintrustConfigState {
    pub fn new(configs: Vec<BraintrustConfig>) -> Self {
        Self {
            snapshot: RwLock::new(BraintrustConfigSnapshot {
                generation: 0,
                configs,
            }),
            tenants: RwLock::new(std::collections::BTreeMap::new()),
            public_egress_only: AtomicBool::new(false),
        }
    }

    pub(crate) fn set_public_egress_only(&self, public_egress_only: bool) {
        self.public_egress_only
            .store(public_egress_only, Ordering::Relaxed);
    }

    pub(crate) fn public_egress_only(&self) -> bool {
        self.public_egress_only.load(Ordering::Relaxed)
    }

    pub(crate) fn register_environment(
        &self,
        environment: &str,
        state: Arc<Self>,
        store: SharedObjectStore,
        resource: Resource,
    ) {
        self.tenants
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                environment.into(),
                BraintrustTenant {
                    state,
                    store,
                    resource,
                },
            );
    }

    fn generation_for_environment(&self, environment: &str) -> Option<i64> {
        let tenants = self
            .tenants
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if tenants.is_empty() {
            return self.generation_if_configured();
        }
        tenants
            .get(environment)
            .and_then(|tenant| tenant.state.generation_if_configured())
    }

    pub(crate) fn generation_if_configured(&self) -> Option<i64> {
        match self.snapshot.read() {
            Ok(snapshot) => (!snapshot.configs.is_empty()).then_some(snapshot.generation),
            Err(poisoned) => {
                tracing::error!(
                    "Braintrust configuration lock was poisoned; recovering the latest snapshot"
                );
                let snapshot = poisoned.into_inner();
                let generation = (!snapshot.configs.is_empty()).then_some(snapshot.generation);
                self.snapshot.clear_poison();
                generation
            }
        }
    }

    pub(crate) fn snapshot(&self) -> (i64, Vec<BraintrustConfig>) {
        match self.snapshot.read() {
            Ok(snapshot) => (snapshot.generation, snapshot.configs.clone()),
            Err(poisoned) => {
                tracing::error!(
                    "Braintrust configuration lock was poisoned; recovering the latest snapshot"
                );
                let snapshot = poisoned.into_inner();
                let result = (snapshot.generation, snapshot.configs.clone());
                self.snapshot.clear_poison();
                result
            }
        }
    }

    pub(crate) fn replace(&self, configs: Vec<BraintrustConfig>) -> Option<Vec<BraintrustConfig>> {
        let mut snapshot = match self.snapshot.write() {
            Ok(current) => current,
            Err(poisoned) => {
                tracing::error!(
                    "Braintrust configuration lock was poisoned; recovering the latest snapshot"
                );
                let current = poisoned.into_inner();
                self.snapshot.clear_poison();
                current
            }
        };
        if snapshot.configs == configs {
            return None;
        }
        snapshot.generation = snapshot.generation.wrapping_add(1);
        Some(std::mem::replace(&mut snapshot.configs, configs))
    }

    #[cfg(test)]
    pub(crate) fn poison_configs(&self) {
        let _guard = self.snapshot.write().unwrap();
        panic!("poison the test lock");
    }
}

/// Everything needed to reach one Braintrust project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BraintrustConfig {
    /// Fully-qualified OTLP trace ingest URL.
    pub traces_url: String,
    /// `BRAINTRUST_API_KEY`, sent as a bearer token.
    pub api_key: String,
    /// `x-bt-parent` value: `project_name:<name>` or `project_id:<uuid>`.
    pub parent: String,
}

impl BraintrustConfig {
    /// Resolve the exporter's configuration from the process environment.
    ///
    /// `None` whenever `BRAINTRUST_API_KEY` is unset or empty. Export is
    /// opt-in: an empty initial snapshot sends and queues no Braintrust spans.
    /// The shared dynamic processor remains installed so a destination added
    /// through the settings service can take effect without a restart.
    pub fn from_env() -> Option<Self> {
        Self::resolve(|key| std::env::var(key).ok())
    }

    pub fn from_parts(
        api_key: String,
        parent: String,
        project: String,
        project_id: String,
        api_url: String,
    ) -> Self {
        let parent = if !parent.trim().is_empty() {
            parent.trim().to_string()
        } else if !project_id.trim().is_empty() {
            format!("project_id:{}", project_id.trim())
        } else {
            format!(
                "project_name:{}",
                if project.trim().is_empty() {
                    DEFAULT_PROJECT
                } else {
                    project.trim()
                }
            )
        };
        Self {
            traces_url: traces_url(if api_url.trim().is_empty() {
                DEFAULT_API_URL.to_string()
            } else {
                api_url.trim().to_string()
            }),
            api_key,
            parent,
        }
    }

    /// [`Self::from_env`]'s body against an arbitrary variable lookup, so the
    /// precedence rules are testable without mutating the process environment
    /// (which is global, and racy across concurrently-running tests).
    fn resolve(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let nonempty = |key: &str| {
            get(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        let api_key = nonempty("BRAINTRUST_API_KEY")?;

        // `BRAINTRUST_PARENT` is the escape hatch: it is passed through
        // verbatim, so a deployment can target an experiment or any other
        // parent kind Braintrust grows without this code changing.
        let parent = nonempty("BRAINTRUST_PARENT")
            .or_else(|| nonempty("BRAINTRUST_PROJECT_ID").map(|id| format!("project_id:{id}")))
            .unwrap_or_else(|| {
                let project =
                    nonempty("BRAINTRUST_PROJECT").unwrap_or_else(|| DEFAULT_PROJECT.to_string());
                format!("project_name:{project}")
            });

        let traces_url = traces_url(
            nonempty("BRAINTRUST_API_URL").unwrap_or_else(|| DEFAULT_API_URL.to_string()),
        );

        Some(Self {
            traces_url,
            api_key,
            parent,
        })
    }
}

/// Derive the OTLP ingest URL from a configured API URL.
///
/// A URL that already names a `/v1/traces` collector path is taken as-is (so an
/// operator can point the router at a sidecar collector); anything else is
/// treated as a Braintrust API root and gets [`OTEL_TRACES_PATH`] appended.
fn traces_url(api_url: String) -> String {
    let trimmed = api_url.trim_end_matches('/');
    if trimmed.ends_with("/v1/traces") {
        return trimmed.to_string();
    }
    format!("{trimmed}{OTEL_TRACES_PATH}")
}

/// Build the span processor that ships frontend spans to Braintrust.
///
/// Batching (rather than a `SimpleSpanProcessor`) because export is a network
/// POST on a foreign service and must never sit on a request's critical path.
pub fn span_processor(
    state: Arc<BraintrustConfigState>,
    resource: &Resource,
    object_store: SharedObjectStore,
) -> impl SpanProcessor {
    let exporter = BraintrustSpanExporter::with_object_store(state.clone(), resource, object_store);
    BraintrustSpanProcessor::new(
        BatchSpanProcessor::builder(exporter, runtime::Tokio)
            .with_batch_config(super::trace_batch_config())
            .build(),
        state,
    )
}

/// A [`SpanExporter`] that POSTs OTLP/protobuf to Braintrust's ingest endpoint.
pub struct BraintrustSpanExporter {
    client: reqwest::Client,
    /// Used for environments whose destinations a tenant chooses. Built on first
    /// use, since only cluster routers need it.
    guarded_client: Arc<OnceLock<reqwest::Client>>,
    state: Arc<BraintrustConfigState>,
    object_store: SharedObjectStore,
    /// Resource attributes stamped on every exported batch.
    resource: Resource,
}

impl std::fmt::Debug for BraintrustSpanExporter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BraintrustSpanExporter")
            .field("object_store_configured", &self.object_store.is_some())
            .finish_non_exhaustive()
    }
}

impl BraintrustSpanExporter {
    /// Public so tests can drive the same dynamic exporter used in production.
    pub fn new(state: Arc<BraintrustConfigState>, resource: &Resource) -> Self {
        Self::with_object_store(state, resource, None)
    }

    pub fn with_object_store(
        state: Arc<BraintrustConfigState>,
        resource: &Resource,
        object_store: SharedObjectStore,
    ) -> Self {
        Self {
            // A dedicated client, not the router's shared one: exports must not
            // contend with in-flight provider requests for pooled connections.
            client: reqwest::Client::new(),
            guarded_client: Arc::new(OnceLock::new()),
            state,
            object_store,
            resource: resource.clone(),
        }
    }
}

impl SpanExporter for BraintrustSpanExporter {
    fn export(&mut self, mut batch: Vec<SpanData>) -> BoxFuture<'static, ExportResult> {
        let tenants = self
            .state
            .tenants
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !tenants.is_empty() {
            let mut exports = Vec::new();
            for (environment, spans) in super::tenant_export::group(batch) {
                let Some(tenant) = tenants.get(&environment) else {
                    continue;
                };
                let client = if tenant.state.public_egress_only() {
                    self.guarded_client
                        .get_or_init(|| crate::egress::new_guarded_client(EXPORT_TIMEOUT))
                        .clone()
                } else {
                    self.client.clone()
                };
                let mut exporter = Self {
                    client,
                    guarded_client: self.guarded_client.clone(),
                    state: tenant.state.clone(),
                    object_store: tenant.store.clone(),
                    resource: tenant.resource.clone(),
                };
                exports.push(exporter.export(spans));
            }
            return Box::pin(async move {
                for result in join_all(exports).await {
                    result?;
                }
                Ok(())
            });
        }
        drop(tenants);
        let (generation, configs) = self.state.snapshot();
        if configs.is_empty() {
            return Box::pin(async { Ok(()) });
        }
        let had_spans = !batch.is_empty();
        batch.retain_mut(|span| span_matches_config_generation(span, generation));
        if had_spans && batch.is_empty() {
            return Box::pin(async { Ok(()) });
        }

        let client = self.client.clone();
        let object_store = self.object_store.clone();
        let resource = self.resource.clone();
        Box::pin(async move {
            tokio::time::timeout(EXPORT_TIMEOUT, async move {
                if !completes_within(
                    HYDRATION_TIMEOUT,
                    hydrate_content_uris(&mut batch, &object_store),
                )
                .await
                {
                    tracing::warn!(
                        timeout_ms = HYDRATION_TIMEOUT.as_millis(),
                        "Skipping Braintrust span-content hydration after timeout"
                    );
                }

                let resource: ResourceAttributesWithSchema = (&resource).into();
                let request = ExportTraceServiceRequest {
                    resource_spans: group_spans_by_resource_and_scope(batch, &resource),
                };
                let body = bytes::Bytes::from(request.encode_to_vec());
                post_batch(client, configs, body).await
            })
            .await
            .map_err(|_| TraceError::from("braintrust span export timed out"))?
        })
    }
}

/// Verify a Braintrust endpoint, credential, and parent without persisting the
/// draft. Braintrust rejects an empty OTLP request, so this sends one synthetic
/// span that is explicitly marked as a telemetry preflight.
///
/// `public_egress_only` probes through the client that refuses private
/// destinations, for an environment whose destination a tenant chose.
pub async fn preflight(config: BraintrustConfig, public_egress_only: bool) -> ExportResult {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64;
    let trace_id = uuid::Uuid::new_v4();
    let span_id = uuid::Uuid::new_v4();
    let request = ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(ProtoResource {
                attributes: vec![string_attribute("service.name", "chalk-router")],
                ..Default::default()
            }),
            scope_spans: vec![ScopeSpans {
                scope: Some(InstrumentationScope {
                    name: "chalk-router.telemetry-preflight".to_string(),
                    ..Default::default()
                }),
                spans: vec![ProtoSpan {
                    trace_id: trace_id.as_bytes().to_vec(),
                    span_id: span_id.as_bytes()[..8].to_vec(),
                    name: "chalk-router.telemetry-preflight".to_string(),
                    kind: span::SpanKind::Internal as i32,
                    start_time_unix_nano: now,
                    end_time_unix_nano: now.saturating_add(1),
                    attributes: vec![bool_attribute("chalk.telemetry.preflight", true)],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    let body = bytes::Bytes::from(request.encode_to_vec());
    let client = if public_egress_only {
        crate::egress::new_guarded_client(EXPORT_TIMEOUT)
    } else {
        reqwest::Client::new()
    };
    tokio::time::timeout(EXPORT_TIMEOUT, post_batch(client, vec![config], body))
        .await
        .map_err(|_| TraceError::from("braintrust preflight timed out"))?
}

fn string_attribute(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.to_string())),
        }),
    }
}

fn bool_attribute(key: &str, value: bool) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::BoolValue(value)),
        }),
    }
}

async fn post_batch(
    client: reqwest::Client,
    configs: Vec<BraintrustConfig>,
    body: bytes::Bytes,
) -> ExportResult {
    let results = join_all(configs.into_iter().map(|config| {
        let client = client.clone();
        let body = body.clone();
        async move {
            let response = client
                .post(&config.traces_url)
                .header(reqwest::header::CONTENT_TYPE, "application/x-protobuf")
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {}", config.api_key),
                )
                .header("x-bt-parent", config.parent)
                .body(body)
                .send()
                .await
                .map_err(|error| format!("{}: {error}", config.traces_url))?;

            let status = response.status();
            if status.is_success() {
                return Ok(());
            }

            let mut detail = response.text().await.unwrap_or_default();
            truncate_error_detail(&mut detail);
            Err(format!(
                "{} rejected with {status}: {detail}",
                config.traces_url
            ))
        }
    }))
    .await;

    let errors: Vec<_> = results.into_iter().filter_map(Result::err).collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(TraceError::from(format!(
            "braintrust span export failed: {}",
            errors.join("; ")
        )))
    }
}

async fn completes_within<F>(timeout: Duration, future: F) -> bool
where
    F: std::future::Future<Output = ()>,
{
    tokio::time::timeout(timeout, future).await.is_ok()
}

async fn hydrate_content_uris(batch: &mut [SpanData], object_store: &SharedObjectStore) {
    let Some(store) = object_store else { return };
    let mut requests: Vec<(usize, String, String, String)> = Vec::new();
    for (span_index, span) in batch.iter().enumerate() {
        for (value_attribute, mime_attribute, uri_attribute) in [
            (
                "input.value",
                "input.mime_type",
                "chalk.trace.input.content_uri",
            ),
            (
                "output.value",
                "output.mime_type",
                "chalk.trace.output.content_uri",
            ),
        ] {
            if span
                .attributes
                .iter()
                .any(|attribute| attribute.key.as_str() == value_attribute)
            {
                continue;
            }
            let uri = span.attributes.iter().find_map(|attribute| {
                (attribute.key.as_str() == uri_attribute).then(|| attribute.value.to_string())
            });
            let Some(uri) = uri else { continue };
            requests.push((
                span_index,
                value_attribute.to_string(),
                mime_attribute.to_string(),
                uri,
            ));
        }
    }

    let hydrated = stream::iter(requests.into_iter().map(
        |(span_index, value_attribute, mime_attribute, uri)| {
            let store = store.clone();
            async move {
                let bytes = store
                    .read_uri_limited(&uri, MAX_HYDRATED_CONTENT_BYTES)
                    .await
                    .ok()??;
                let json = String::from_utf8(bytes.to_vec()).ok()?;
                Some((span_index, value_attribute, mime_attribute, json))
            }
        },
    ))
    .buffer_unordered(MAX_CONCURRENT_HYDRATIONS)
    .filter_map(|result| async move { result })
    .collect::<Vec<_>>()
    .await;

    for (span_index, value_attribute, mime_attribute, json) in hydrated {
        if let Some(span) = batch.get_mut(span_index) {
            span.attributes
                .push(opentelemetry::KeyValue::new(value_attribute, json));
            span.attributes.push(opentelemetry::KeyValue::new(
                mime_attribute,
                "application/json",
            ));
        }
    }
}

/// Wraps a span processor so it only sees the router's frontend spans.
///
/// `on_start` still reaches the inner processor for every span — the batch
/// processor does no work there, and skipping it would be a lie about the
/// processor contract. The filtering happens at `on_end`, where a span's
/// attributes are final.
#[derive(Debug)]
pub struct BraintrustSpanProcessor<P: SpanProcessor> {
    inner: Arc<P>,
    state: Arc<BraintrustConfigState>,
}

impl<P: SpanProcessor> BraintrustSpanProcessor<P> {
    pub fn new(inner: P, state: Arc<BraintrustConfigState>) -> Self {
        Self {
            inner: Arc::new(inner),
            state,
        }
    }
}

impl<P: SpanProcessor> SpanProcessor for BraintrustSpanProcessor<P> {
    fn on_start(&self, span: &mut Span, cx: &Context) {
        self.inner.on_start(span, cx);
    }

    fn on_end(&self, mut span: SpanData) {
        let scan = scan_frontend_span(&span);
        if !scan.is_frontend {
            return;
        }
        let Some(generation) = self
            .state
            .generation_for_environment(super::tenant_export::environment(&span))
        else {
            return;
        };
        add_completion_token_metric(&mut span, &scan);
        stamp_config_generation(&mut span, generation);
        self.inner.on_end(span);
    }

    fn force_flush(&self) -> TraceResult<()> {
        self.inner.force_flush()
    }

    fn shutdown(&self) -> TraceResult<()> {
        self.inner.shutdown()
    }

    fn set_resource(&mut self, _resource: &Resource) {
        // The exporter receives the provider resource before this processor is
        // constructed, avoiding shared mutability on the request path.
    }
}

fn stamp_config_generation(span: &mut SpanData, generation: i64) {
    span.attributes
        .retain(|attribute| attribute.key.as_str() != CONFIG_GENERATION_ATTRIBUTE);
    span.attributes.push(opentelemetry::KeyValue::new(
        CONFIG_GENERATION_ATTRIBUTE,
        generation,
    ));
}

fn take_config_generation(span: &mut SpanData) -> Option<i64> {
    let index = span
        .attributes
        .iter()
        .position(|attribute| attribute.key.as_str() == CONFIG_GENERATION_ATTRIBUTE)?;
    match span.attributes.swap_remove(index).value {
        opentelemetry::Value::I64(generation) => Some(generation),
        _ => None,
    }
}

fn span_matches_config_generation(span: &mut SpanData, generation: i64) -> bool {
    take_config_generation(span) == Some(generation)
}

fn truncate_error_detail(detail: &mut String) {
    if detail.len() <= ERROR_BODY_LIMIT {
        return;
    }
    let mut end = ERROR_BODY_LIMIT;
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    detail.truncate(end);
}

/// OpenInference's completion-token count.
const COMPLETION_TOKENS_ATTRIBUTE: &str = "llm.token_count.completion";

/// Braintrust's own escape hatch: a JSON object merged into a span's metrics.
const BRAINTRUST_METRICS_ATTRIBUTE: &str = "braintrust.metrics";

/// Everything `on_end` needs to know about a span's attributes, gathered by
/// [`scan_frontend_span`] in one pass instead of the separate linear scans
/// `is_frontend_span` and `add_completion_token_metric` used to each run over
/// the same `Vec` on every span.
struct FrontendSpanScan {
    is_frontend: bool,
    has_braintrust_metrics: bool,
    completion_tokens: Option<i64>,
    background_metrics: serde_json::Map<String, serde_json::Value>,
}

/// Scan a finished span's attributes once for everything `on_end` needs: is it
/// worth a Braintrust row at all, does it already carry Braintrust's own
/// metrics, and what completion-token count (if any) it reports.
///
/// Two markers say a span belongs to one of the router's LLM frontends,
/// deliberately: `chalk.frontend` catches the per-request span of an inbound
/// LLM route (stamped by [`crate::telemetry::OtelMakeSpan`]) so each
/// Braintrust trace has a root describing the API call, and
/// `openinference.span.kind` catches every span already instrumented for LLM
/// observability — the handlers' `chat.completion` / `anthropic.messages` /
/// embedding spans, and the agent spans replayed through `/v1/traces/spans`.
fn scan_frontend_span(span: &SpanData) -> FrontendSpanScan {
    let mut scan = FrontendSpanScan {
        is_frontend: false,
        has_braintrust_metrics: false,
        completion_tokens: None,
        background_metrics: serde_json::Map::new(),
    };
    for kv in &span.attributes {
        match kv.key.as_str() {
            FRONTEND_ATTRIBUTE | OPENINFERENCE_SPAN_KIND_ATTRIBUTE => scan.is_frontend = true,
            BRAINTRUST_METRICS_ATTRIBUTE => scan.has_braintrust_metrics = true,
            COMPLETION_TOKENS_ATTRIBUTE => {
                if let opentelemetry::Value::I64(count) = kv.value {
                    scan.completion_tokens = Some(count);
                }
            }
            "chalk.background.poll_attempts" | "chalk.background.settlement_delay_ms" => {
                if let opentelemetry::Value::I64(value) = kv.value {
                    let name = match kv.key.as_str() {
                        "chalk.background.poll_attempts" => "background_poll_attempts",
                        _ => "background_settlement_delay_ms",
                    };
                    scan.background_metrics
                        .insert(name.to_string(), value.into());
                }
            }
            _ => {}
        }
    }
    scan
}

/// Carry completion tokens into Braintrust's metrics.
///
/// Braintrust's OpenInference translation reads `llm.token_count.prompt` and
/// `.total` into a span's `metrics` — but not `.completion`. Left alone, the
/// expensive half of every call arrives as inert metadata and is missing from
/// the token and cost views that are the reason to send spans there at all.
/// `braintrust.metrics` is merged into (not substituted for) whatever their
/// translation derives, so filling in the single field it drops is enough.
///
/// A span that already carries `braintrust.metrics` — one replayed through
/// `/v1/traces/spans` by a client that speaks Braintrust directly — is left
/// alone; its author knows better than this translation does. `scan` is the
/// result of [`scan_frontend_span`], already run once over this span by the
/// caller, so this only mutates — it never re-scans `span.attributes`.
fn add_completion_token_metric(span: &mut SpanData, scan: &FrontendSpanScan) {
    if scan.has_braintrust_metrics {
        return;
    }
    let mut metrics = scan.background_metrics.clone();
    if let Some(count) = scan.completion_tokens {
        metrics.insert("completion_tokens".to_string(), count.into());
    }
    if metrics.is_empty() {
        return;
    }
    span.attributes.push(opentelemetry::KeyValue::new(
        BRAINTRUST_METRICS_ATTRIBUTE,
        serde_json::Value::Object(metrics).to_string(),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{
        SpanContext, SpanId, SpanKind, Status, TraceFlags, TraceId, TraceState,
    };
    use opentelemetry::{InstrumentationScope, KeyValue};
    use std::borrow::Cow;
    use std::collections::HashMap;
    use std::time::SystemTime;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    fn span_with_attributes(attributes: Vec<KeyValue>) -> SpanData {
        SpanData {
            span_context: SpanContext::new(
                TraceId::from_u128(1),
                SpanId::from_u64(1),
                TraceFlags::SAMPLED,
                false,
                TraceState::default(),
            ),
            parent_span_id: SpanId::INVALID,
            span_kind: SpanKind::Server,
            name: Cow::Borrowed("span"),
            start_time: SystemTime::UNIX_EPOCH,
            end_time: SystemTime::UNIX_EPOCH,
            attributes,
            dropped_attributes_count: 0,
            events: Default::default(),
            links: Default::default(),
            status: Status::Unset,
            instrumentation_scope: InstrumentationScope::default(),
        }
    }

    #[test]
    fn disabled_without_an_api_key() {
        assert_eq!(BraintrustConfig::resolve(env(&[])), None);
        assert_eq!(
            BraintrustConfig::resolve(env(&[("BRAINTRUST_API_KEY", "  ")])),
            None
        );
        // A project alone is not enough to start exporting.
        assert_eq!(
            BraintrustConfig::resolve(env(&[("BRAINTRUST_PROJECT", "p")])),
            None
        );
    }

    #[test]
    fn defaults_to_the_hosted_api_and_service_named_project() {
        let config = BraintrustConfig::resolve(env(&[("BRAINTRUST_API_KEY", "sk-test")])).unwrap();

        assert_eq!(config.api_key, "sk-test");
        assert_eq!(config.parent, "project_name:chalk-router");
        assert_eq!(
            config.traces_url,
            "https://api.braintrust.dev/otel/v1/traces"
        );
    }

    #[test]
    fn parent_precedence_is_explicit_then_id_then_name() {
        let by_name = BraintrustConfig::resolve(env(&[
            ("BRAINTRUST_API_KEY", "sk-test"),
            ("BRAINTRUST_PROJECT", "router-prod"),
        ]))
        .unwrap();
        assert_eq!(by_name.parent, "project_name:router-prod");

        // An id is more specific than a name, so it wins.
        let by_id = BraintrustConfig::resolve(env(&[
            ("BRAINTRUST_API_KEY", "sk-test"),
            ("BRAINTRUST_PROJECT", "router-prod"),
            ("BRAINTRUST_PROJECT_ID", "abc-123"),
        ]))
        .unwrap();
        assert_eq!(by_id.parent, "project_id:abc-123");

        // And an explicit parent overrides both, verbatim.
        let explicit = BraintrustConfig::resolve(env(&[
            ("BRAINTRUST_API_KEY", "sk-test"),
            ("BRAINTRUST_PROJECT", "router-prod"),
            ("BRAINTRUST_PROJECT_ID", "abc-123"),
            ("BRAINTRUST_PARENT", "experiment_id:xyz"),
        ]))
        .unwrap();
        assert_eq!(explicit.parent, "experiment_id:xyz");
    }

    #[test]
    fn stored_explicit_parent_takes_precedence() {
        let config = BraintrustConfig::from_parts(
            "sk-test".to_string(),
            "experiment_id:xyz".to_string(),
            "ignored-name".to_string(),
            "ignored-id".to_string(),
            "https://api.braintrust.dev".to_string(),
        );

        assert_eq!(config.parent, "experiment_id:xyz");
    }

    #[test]
    fn api_url_gains_the_otel_path_unless_it_already_names_a_collector() {
        assert_eq!(
            traces_url("https://api.braintrust.dev".to_string()),
            "https://api.braintrust.dev/otel/v1/traces"
        );
        // A trailing slash must not produce a doubled separator.
        assert_eq!(
            traces_url("https://api.braintrust.dev/".to_string()),
            "https://api.braintrust.dev/otel/v1/traces"
        );
        // An explicit collector URL is taken as given.
        assert_eq!(
            traces_url("http://localhost:4318/v1/traces".to_string()),
            "http://localhost:4318/v1/traces"
        );
    }

    #[tokio::test]
    async fn hydrates_uploaded_content_for_braintrust_only() {
        let store = std::sync::Arc::new(crate::storage::ObjectStore::memory());
        let json = format!(r#"{{"prompt":"{}"}}"#, "x".repeat(70 * 1024));
        let key = store.put_hashed(json.as_bytes(), "json").await.unwrap();
        let uri = store.uri_for_key(&key);
        let mut spans = vec![span_with_attributes(vec![KeyValue::new(
            "chalk.trace.input.content_uri",
            uri,
        )])];

        hydrate_content_uris(&mut spans, &Some(store)).await;

        assert!(spans[0].attributes.iter().any(|attribute| {
            attribute.key.as_str() == "input.value" && attribute.value.as_str() == json
        }));
        assert!(spans[0].attributes.iter().any(|attribute| {
            attribute.key.as_str() == "input.mime_type"
                && attribute.value.as_str() == "application/json"
        }));
    }

    #[tokio::test]
    async fn hydration_timeout_leaves_the_bounded_span_unchanged() {
        let mut spans = [span_with_attributes(vec![KeyValue::new(
            "chalk.trace.input.content_uri",
            "memory:///2026-08-24/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.json",
        )])];

        let completed = completes_within(Duration::ZERO, async {
            std::future::pending::<()>().await;
            spans[0]
                .attributes
                .push(KeyValue::new("input.value", "should never be written"));
        })
        .await;

        assert!(!completed);
        assert!(spans[0]
            .attributes
            .iter()
            .all(|attribute| attribute.key.as_str() != "input.value"));
    }

    #[test]
    fn the_otlp_endpoint_env_var_cannot_redirect_the_exporter() {
        // The router sets OTEL_EXPORTER_OTLP_ENDPOINT in every deployment;
        // Braintrust's endpoint must come from Braintrust's own config only.
        let config = BraintrustConfig::resolve(env(&[
            ("BRAINTRUST_API_KEY", "sk-test"),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "https://telemetry.chalk.ai"),
            (
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                "https://telemetry.chalk.ai/v1/traces",
            ),
        ]))
        .unwrap();

        assert_eq!(
            config.traces_url,
            "https://api.braintrust.dev/otel/v1/traces"
        );
    }

    #[test]
    fn error_details_are_truncated_on_a_utf8_boundary() {
        let mut detail = format!("{}é-tail", "x".repeat(ERROR_BODY_LIMIT - 1));
        truncate_error_detail(&mut detail);

        assert!(detail.len() <= ERROR_BODY_LIMIT);
        assert_eq!(detail, "x".repeat(ERROR_BODY_LIMIT - 1));
    }

    #[test]
    fn poisoned_config_locks_recover_the_last_snapshot() {
        let state = Arc::new(BraintrustConfigState::new(vec![BraintrustConfig {
            traces_url: "https://api.braintrust.dev/otel/v1/traces".to_string(),
            api_key: "secret".to_string(),
            parent: "project_name:test".to_string(),
        }]));
        let poison = state.clone();
        assert!(std::thread::spawn(move || poison.poison_configs())
            .join()
            .is_err());

        assert!(state.snapshot.is_poisoned());
        assert_eq!(state.snapshot().1.len(), 1);
        assert!(!state.snapshot.is_poisoned());
        assert_eq!(state.snapshot().1.len(), 1);
    }

    #[tokio::test]
    async fn exporter_drops_spans_from_an_old_generation() {
        let old = BraintrustConfig {
            traces_url: "https://old.example/otel/v1/traces".to_string(),
            api_key: "old-secret".to_string(),
            parent: "project_name:old".to_string(),
        };
        let state = Arc::new(BraintrustConfigState::new(vec![old]));
        let mut exporter = BraintrustSpanExporter::new(state.clone(), &Resource::empty());
        let generation = state.generation_if_configured().unwrap();
        let mut span = span_with_attributes(vec![
            KeyValue::new(FRONTEND_ATTRIBUTE, "openai"),
            // A client-provided marker must not be able to opt an old span
            // into a future destination generation.
            KeyValue::new(CONFIG_GENERATION_ATTRIBUTE, generation.wrapping_add(1)),
        ]);
        stamp_config_generation(&mut span, generation);

        state.replace(vec![BraintrustConfig {
            // This is intentionally invalid: any attempted export would fail.
            traces_url: "not-a-valid-url".to_string(),
            api_key: "new-secret".to_string(),
            parent: "project_name:new".to_string(),
        }]);
        let (current_generation, configs) = state.snapshot();

        assert_eq!(configs[0].parent, "project_name:new");
        assert_ne!(generation, current_generation);
        exporter
            .export(vec![span])
            .await
            .expect("a stale span must be dropped before any request is built");
    }

    #[test]
    fn forwards_llm_and_frontend_spans() {
        assert!(
            scan_frontend_span(&span_with_attributes(vec![KeyValue::new(
                OPENINFERENCE_SPAN_KIND_ATTRIBUTE,
                "LLM"
            )]))
            .is_frontend
        );
        assert!(
            scan_frontend_span(&span_with_attributes(vec![KeyValue::new(
                FRONTEND_ATTRIBUTE,
                "openai"
            )]))
            .is_frontend
        );
        // The agent spans replayed through /v1/traces/spans carry non-LLM kinds
        // and belong in Braintrust too.
        assert!(
            scan_frontend_span(&span_with_attributes(vec![KeyValue::new(
                OPENINFERENCE_SPAN_KIND_ATTRIBUTE,
                "TOOL"
            )]))
            .is_frontend
        );
    }

    fn braintrust_metrics(span: &SpanData) -> Option<String> {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == BRAINTRUST_METRICS_ATTRIBUTE)
            .map(|kv| kv.value.to_string())
    }

    #[test]
    fn background_metrics_preserve_completion_tokens_and_client_metrics() {
        let mut span = span_with_attributes(vec![
            KeyValue::new(OPENINFERENCE_SPAN_KIND_ATTRIBUTE, "LLM"),
            KeyValue::new(COMPLETION_TOKENS_ATTRIBUTE, 7_i64),
            KeyValue::new("chalk.background.poll_attempts", 2_i64),
            KeyValue::new("chalk.background.settlement_delay_ms", 1500_i64),
        ]);
        let scan = scan_frontend_span(&span);
        add_completion_token_metric(&mut span, &scan);
        let metrics: serde_json::Value =
            serde_json::from_str(&braintrust_metrics(&span).unwrap()).unwrap();
        assert_eq!(
            metrics,
            serde_json::json!({"completion_tokens":7,"background_poll_attempts":2,"background_settlement_delay_ms":1500})
        );
        let mut client = span_with_attributes(vec![
            KeyValue::new(BRAINTRUST_METRICS_ATTRIBUTE, r#"{"custom":9}"#),
            KeyValue::new("chalk.background.poll_attempts", 2_i64),
        ]);
        let scan = scan_frontend_span(&client);
        add_completion_token_metric(&mut client, &scan);
        assert_eq!(braintrust_metrics(&client).unwrap(), r#"{"custom":9}"#);
    }

    #[test]
    fn completion_tokens_are_carried_into_braintrusts_metrics() {
        // Braintrust reads prompt/total out of OpenInference itself, so only
        // the field its translation drops is filled in.
        let mut span = span_with_attributes(vec![
            KeyValue::new(OPENINFERENCE_SPAN_KIND_ATTRIBUTE, "LLM"),
            KeyValue::new("llm.token_count.prompt", 11_i64),
            KeyValue::new(COMPLETION_TOKENS_ATTRIBUTE, 6_i64),
            KeyValue::new("llm.token_count.total", 17_i64),
        ]);
        let scan = scan_frontend_span(&span);
        add_completion_token_metric(&mut span, &scan);

        assert_eq!(
            braintrust_metrics(&span).as_deref(),
            Some(r#"{"completion_tokens":6}"#)
        );
    }

    #[test]
    fn spans_without_completion_tokens_are_left_alone() {
        // Embedding spans report no completion tokens, and neither does an LLM
        // span whose upstream omitted usage.
        let mut span = span_with_attributes(vec![
            KeyValue::new(OPENINFERENCE_SPAN_KIND_ATTRIBUTE, "EMBEDDING"),
            KeyValue::new("llm.token_count.prompt", 11_i64),
        ]);
        let scan = scan_frontend_span(&span);
        add_completion_token_metric(&mut span, &scan);

        assert_eq!(braintrust_metrics(&span), None);
    }

    #[test]
    fn a_clients_own_braintrust_metrics_are_not_overwritten() {
        let mut span = span_with_attributes(vec![
            KeyValue::new(OPENINFERENCE_SPAN_KIND_ATTRIBUTE, "LLM"),
            KeyValue::new(COMPLETION_TOKENS_ATTRIBUTE, 6_i64),
            KeyValue::new(BRAINTRUST_METRICS_ATTRIBUTE, r#"{"completion_tokens":99}"#),
        ]);
        let scan = scan_frontend_span(&span);
        add_completion_token_metric(&mut span, &scan);

        assert_eq!(
            braintrust_metrics(&span).as_deref(),
            Some(r#"{"completion_tokens":99}"#)
        );
    }

    #[test]
    fn drops_the_routers_own_infrastructure_spans() {
        // An admin RPC's request span: no frontend marker, no LLM kind.
        assert!(
            !scan_frontend_span(&span_with_attributes(vec![
                KeyValue::new("http.method", "POST"),
                KeyValue::new("http.target", "/v1/admin/keys"),
            ]))
            .is_frontend
        );
        // A cache refresh started deep inside a request.
        assert!(
            !scan_frontend_span(&span_with_attributes(vec![KeyValue::new(
                "cache",
                "fallback_policy"
            )]))
            .is_frontend
        );
        assert!(!scan_frontend_span(&span_with_attributes(vec![])).is_frontend);
    }
}
