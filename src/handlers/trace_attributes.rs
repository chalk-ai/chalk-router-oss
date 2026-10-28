use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use opentelemetry::KeyValue;
use serde_json::{Map, Value};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::storage;

pub const MAX_INLINE_CONTENT_BYTES: usize = 64 * 1024;
pub const MAX_INVOCATION_PARAMETERS_BYTES: usize = 8 * 1024;
pub const MAX_LLM_SPAN_CONTENT_BYTES: usize =
    2 * MAX_INLINE_CONTENT_BYTES + MAX_INVOCATION_PARAMETERS_BYTES;

/// The two expandable attribute pools are intentionally asymmetric: inputs
/// keep one third while outputs keep two thirds so complete result metadata is
/// retained before optional prose. Fixed attributes are reserved separately by
/// [`LLM_FIXED_ATTRIBUTE_SLOTS`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentDirection {
    Input,
    Output,
}

impl ContentDirection {
    fn value_attribute(self) -> &'static str {
        match self {
            Self::Input => "input.value",
            Self::Output => "output.value",
        }
    }

    fn mime_type_attribute(self) -> &'static str {
        match self {
            Self::Input => "input.mime_type",
            Self::Output => "output.mime_type",
        }
    }

    fn content_bytes_attribute(self) -> &'static str {
        match self {
            Self::Input => "chalk.trace.input.content_bytes",
            Self::Output => "chalk.trace.output.content_bytes",
        }
    }

    fn content_uri_attribute(self) -> &'static str {
        match self {
            Self::Input => "chalk.trace.input.content_uri",
            Self::Output => "chalk.trace.output.content_uri",
        }
    }

    fn upload_omitted_attribute(self) -> &'static str {
        match self {
            Self::Input => "chalk.trace.input.upload_omitted",
            Self::Output => "chalk.trace.output.upload_omitted",
        }
    }

    fn truncation_attribute(self) -> &'static str {
        match self {
            Self::Input => "chalk.trace.input.truncation",
            Self::Output => "chalk.trace.output.truncation",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }
}

const MAX_CONCURRENT_CONTENT_UPLOADS: usize = 16;
const MAX_CONTENT_UPLOAD_MEMORY_BYTES: usize = 128 * 1024 * 1024;
const MIN_CONTENT_UPLOAD_MEMORY_BYTES: usize = MAX_INLINE_CONTENT_BYTES + 1;
const FALLBACK_CONTENT_UPLOAD_MEMORY_BYTES: usize = 128 * 1024 * 1024;
const CONTENT_UPLOAD_MEMORY_FRACTION: u64 = 32;
const CONTENT_UPLOAD_ATTEMPTS: usize = 2;
const CONTENT_UPLOAD_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_FIXED_STRING_BYTES: usize = MAX_INVOCATION_PARAMETERS_BYTES;

static CONTENT_UPLOAD_PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_CONTENT_UPLOADS)));
static CONTENT_UPLOAD_BYTES: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug)]
struct PodMemory {
    limit: u64,
}

/// Memory retained by trace-content uploads is capped at 1/32 of the pod's
/// cgroup limit (and 128 MiB), never at a fraction of host/node memory. If the
/// process has no finite cgroup limit (local development, for example), use a
/// fixed 128 MiB fallback instead of consulting `/proc/meminfo`.
fn content_upload_memory_limit(pod_memory: Option<PodMemory>) -> usize {
    pod_memory
        .map(|memory| {
            usize::try_from(memory.limit / CONTENT_UPLOAD_MEMORY_FRACTION)
                .unwrap_or(usize::MAX)
                .clamp(
                    MIN_CONTENT_UPLOAD_MEMORY_BYTES,
                    MAX_CONTENT_UPLOAD_MEMORY_BYTES,
                )
        })
        .unwrap_or(FALLBACK_CONTENT_UPLOAD_MEMORY_BYTES)
}

fn read_cgroup_number(path: &str) -> Option<u64> {
    let value = std::fs::read_to_string(path).ok()?;
    let value = value.trim();
    if value == "max" {
        return None;
    }
    let value = value.parse::<u64>().ok()?;
    // cgroup v1 represents an unlimited controller with a value near i64::MAX.
    (value < (1_u64 << 60)).then_some(value)
}

fn pod_memory() -> Option<PodMemory> {
    // In a container these files are scoped to the container/pod cgroup, not
    // the node. Support both the unified cgroup v2 layout and legacy v1.
    [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ]
    .into_iter()
    .find_map(|limit_path| {
        Some(PodMemory {
            limit: read_cgroup_number(limit_path)?,
        })
    })
}

struct UploadMemoryReservation {
    bytes: usize,
}

impl Drop for UploadMemoryReservation {
    fn drop(&mut self) {
        CONTENT_UPLOAD_BYTES.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

fn try_reserve_upload_memory(bytes: usize, memory_limit: usize) -> Option<UploadMemoryReservation> {
    let mut current = CONTENT_UPLOAD_BYTES.load(Ordering::Acquire);
    loop {
        let next = current.checked_add(bytes)?;
        if next > memory_limit {
            return None;
        }
        match CONTENT_UPLOAD_BYTES.compare_exchange_weak(
            current,
            next,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Some(UploadMemoryReservation { bytes }),
            Err(actual) => current = actual,
        }
    }
}

// Every logical slot that can be written outside the flattened message/tool
// expansions. Keeping this explicit makes the dynamic budgets a function of
// the same configured limit the SDK enforces instead of a hard-coded guess.
// Optional keys still reserve their worst-case slot; the final entries cover
// tracing-opentelemetry source fields and the session-id span processor.
const LLM_FIXED_ATTRIBUTE_SLOTS: &[&str] = &[
    "chalk.environment_id",
    "openinference.span.kind",
    "user.id",
    "chalk.auth.principal.id",
    "chalk.auth.principal.type",
    "llm.invocation_parameters",
    "input.value_or_uri",
    "input.mime_type_or_content_bytes",
    "chalk.trace.input.upload_omitted",
    "chalk.trace.input.truncation",
    "llm.system",
    "llm.model_name",
    "llm.provider",
    "chalk.connection_id",
    "chalk.connection_name",
    "llm.stream",
    "chalk.router.requested_model",
    "chalk.router.served_model",
    "chalk.router.attempt_count",
    "chalk.router.fallback_used",
    "output.value_or_uri",
    "output.mime_type_or_content_bytes",
    "chalk.trace.output.upload_omitted",
    "chalk.trace.output.truncation",
    "llm.finish_reason",
    "llm.token_count.prompt",
    "llm.token_count.completion",
    "llm.token_count.total",
    "llm.token_count.prompt_details.cache_read",
    "llm.token_count.prompt_details.cache_write",
    "llm.token_count.completion_details.reasoning",
    "llm.cost.total",
    "llm.cost.prompt",
    "llm.cost.completion",
    "chalk.cost.source",
    "chalk.cost.rate_card_version",
    "llm.time_to_first_token_ms",
    "chalk.router.cache.status",
    "chalk.router.cache.backend",
    "chalk.router.cache.scope",
    "chalk.router.cache.key",
    "chalk.router.cache.age_secs",
    "chalk.router.judge.classification",
    "chalk.router.judge.selected_model",
    "chalk.router.judge.selected_model_display",
    "chalk.router.judge.fallbacks",
    "chalk.router.judge.shadow",
    "chalk.router.judge.status",
    "chalk.router.judge.classification_type",
    "chalk.router.judge.default_model",
    "chalk.router.judge.default_model_display",
    "chalk.router.judge.policy_revision",
    "chalk.router.judge.backend",
    "chalk.router.judge.latency_ms",
    "chalk.router.judge.cache_hit",
    "chalk.router.judge.reason",
    "error.type",
    "error.message",
    "embedding.input_count",
    "embedding.model_name",
    "embedding.system",
    "embedding.embeddings_count",
    "embedding.vector_dimension",
    "session.id",
    "tracing.code.filepath",
    "tracing.code.lineno",
    "tracing.code.namespace",
    "tracing.target",
];
// tracing-opentelemetry and error/status instrumentation may add attributes
// outside the handler-owned list above. Keep explicit headroom so expandable
// message/tool/image fields cannot consume slots that are already occupied.
const LLM_FIXED_ATTRIBUTE_HEADROOM: usize = 8;

pub fn llm_attribute_budgets() -> (usize, usize) {
    let dynamic = crate::telemetry::span_attribute_count_limit()
        .saturating_sub(LLM_FIXED_ATTRIBUTE_SLOTS.len() + LLM_FIXED_ATTRIBUTE_HEADROOM);
    let input = dynamic / 3;
    (input, dynamic.saturating_sub(input))
}

#[derive(Debug)]
struct AttributeBudget {
    attributes_remaining: usize,
    content_bytes_remaining: usize,
}

impl AttributeBudget {
    fn new(attributes: usize, inline_content_bytes: usize) -> Self {
        Self {
            attributes_remaining: attributes,
            content_bytes_remaining: MAX_INLINE_CONTENT_BYTES.saturating_sub(inline_content_bytes),
        }
    }

    fn can_reserve_attributes(&self, count: usize) -> bool {
        count <= self.attributes_remaining
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LlmSpanKind {
    Llm,
    Embedding,
}

impl LlmSpanKind {
    fn openinference_value(self) -> &'static str {
        match self {
            Self::Llm => "LLM",
            Self::Embedding => "EMBEDDING",
        }
    }
}

struct DirectionState {
    budget: AttributeBudget,
    truncation: Map<String, Value>,
}

impl DirectionState {
    fn new(attributes: usize) -> Self {
        Self {
            budget: AttributeBudget::new(attributes, 0),
            truncation: Map::new(),
        }
    }
}

struct WriterState {
    input: DirectionState,
    output: DirectionState,
    total_content_bytes_remaining: usize,
    fixed_string_bytes: HashMap<&'static str, usize>,
    /// The resolved model and the provider serving it, kept so
    /// `set_token_usage` can price the request. Routed LLM paths populate this
    /// through `set_route`; direct media/embedding paths use
    /// `set_model_system`. The provider is not optional detail -- Bedrock and
    /// Anthropic sell the same models at different rates.
    model: Option<(String, String)>,
    /// Router response-cache hits replay the original token counts for trace
    /// usefulness but make no provider request. Once set, later token stamping
    /// must not replace their exact zero upstream cost with a list-price
    /// estimate for the cached payload.
    provider_cost_suppressed: bool,
    rate_card_version: Option<String>,
}

impl WriterState {
    fn direction(&self, direction: ContentDirection) -> &DirectionState {
        match direction {
            ContentDirection::Input => &self.input,
            ContentDirection::Output => &self.output,
        }
    }

    fn direction_mut(&mut self, direction: ContentDirection) -> &mut DirectionState {
        match direction {
            ContentDirection::Input => &mut self.input,
            ContentDirection::Output => &mut self.output,
        }
    }

    fn reserve_dynamic(
        &mut self,
        direction: ContentDirection,
        attributes: usize,
        bytes: usize,
        reserved_attributes: usize,
        reserved_content_bytes: usize,
    ) -> bool {
        let available = self.direction(direction);
        if attributes
            > available
                .budget
                .attributes_remaining
                .saturating_sub(reserved_attributes)
            || bytes
                > available
                    .budget
                    .content_bytes_remaining
                    .saturating_sub(reserved_content_bytes)
            || bytes > self.total_content_bytes_remaining
        {
            return false;
        }
        let budget = &mut self.direction_mut(direction).budget;
        budget.attributes_remaining -= attributes;
        budget.content_bytes_remaining -= bytes;
        self.total_content_bytes_remaining -= bytes;
        true
    }

    fn reserve_fixed_content(&mut self, direction: ContentDirection, bytes: usize) -> bool {
        if bytes > self.direction(direction).budget.content_bytes_remaining
            || bytes > self.total_content_bytes_remaining
        {
            return false;
        }
        self.direction_mut(direction).budget.content_bytes_remaining -= bytes;
        self.total_content_bytes_remaining -= bytes;
        true
    }

    fn reserve_bounded_fixed_string(&mut self, key: &'static str, bytes: usize) -> bool {
        let previous = self.fixed_string_bytes.get(key).copied().unwrap_or(0);
        let available = self.total_content_bytes_remaining.saturating_add(previous);
        if bytes > available {
            return false;
        }
        self.total_content_bytes_remaining = available - bytes;
        self.fixed_string_bytes.insert(key, bytes);
        true
    }
}

/// The only write-capable handle for an LLM/embedding span.
///
/// The raw span is deliberately private. Every string received from a client
/// or provider must therefore choose either the metadata or content path; both
/// are byte-accounted, while the content path is also disabled by
/// `ROUTER_TRACE_LOG_CONTENT=false`. Clones share one allocator so streaming
/// finalizers cannot reset the request's budget.
#[derive(Clone)]
pub struct LlmSpanWriter {
    span: tracing::Span,
    object_store: storage::SharedObjectStore,
    log_content: bool,
    state: Arc<Mutex<WriterState>>,
}

pub(super) struct ReservedLlmAttributes {
    fields: Vec<(String, String)>,
}

/// Marks spans a client reported rather than the router measured, so
/// downstream consumers can tell client-reported token counts from metered ones.
pub(crate) const SPAN_SOURCE_ATTRIBUTE: &str = "chalk.span.source";

/// Attributes the router stamps on every client-ingested span, after the
/// client's own attributes have passed [`LlmSpanWriter::apply_client_content_policy`]:
/// the span source, and the caller identity auth verified. Static admin
/// credentials and anonymous requests carry no principal; only federated users
/// carry a user ID. Oversized identity values are omitted, as on LLM spans.
pub(crate) fn client_span_router_attributes(
    principal_id: Option<&str>,
    principal_type: Option<&str>,
    user_id: Option<&str>,
) -> Vec<KeyValue> {
    let bounded = |value: Option<&str>| {
        value
            .filter(|value| value.len() <= MAX_FIXED_STRING_BYTES)
            .map(str::to_owned)
    };
    let mut attributes = vec![KeyValue::new(SPAN_SOURCE_ATTRIBUTE, "client")];
    if let (Some(id), Some(kind)) = (bounded(principal_id), bounded(principal_type)) {
        attributes.push(KeyValue::new("chalk.auth.principal.id", id));
        attributes.push(KeyValue::new("chalk.auth.principal.type", kind));
    }
    if let Some(user) = bounded(user_id) {
        attributes.push(KeyValue::new("user.id", user));
    }
    attributes
}

impl LlmSpanWriter {
    /// Client attributes have an open vocabulary. With content disabled, only
    /// explicit non-content metadata survives; a deny-list cannot cover custom
    /// prompt, tool, retrieval or template attribute spellings.
    pub fn apply_client_content_policy(attributes: &mut Vec<KeyValue>, log_content: bool) {
        if log_content && super::content_logging_enabled() {
            return;
        }
        attributes.retain(|kv| {
            matches!(
                kv.key.as_str(),
                "openinference.span.kind"
                    | "session.id"
                    | "llm.model_name"
                    | "llm.system"
                    | "llm.provider"
                    | "embedding.model_name"
                    | "llm.token_count.prompt"
                    | "llm.token_count.completion"
                    | "llm.token_count.total"
                    | "llm.token_count.prompt_details.cache_read"
                    | "llm.token_count.prompt_details.cache_write"
                    | "llm.token_count.completion_details.reasoning"
            )
        });
    }

    pub fn new(
        span: tracing::Span,
        object_store: storage::SharedObjectStore,
        kind: LlmSpanKind,
    ) -> Self {
        let (input_attributes, output_attributes) = llm_attribute_budgets();
        let writer = Self {
            span,
            object_store,
            log_content: super::content_logging_enabled(),
            state: Arc::new(Mutex::new(WriterState {
                input: DirectionState::new(input_attributes),
                output: DirectionState::new(output_attributes),
                total_content_bytes_remaining: MAX_LLM_SPAN_CONTENT_BYTES,
                fixed_string_bytes: HashMap::new(),
                model: None,
                provider_cost_suppressed: false,
                rate_card_version: None,
            })),
        };
        writer
            .span
            .set_attribute("openinference.span.kind", kind.openinference_value());
        writer
    }

    #[cfg(test)]
    pub(crate) fn new_with_limits(
        span: tracing::Span,
        kind: LlmSpanKind,
        input_attributes: usize,
        output_attributes: usize,
        content_bytes: usize,
        log_content: bool,
    ) -> Self {
        let writer = Self {
            span,
            object_store: None,
            log_content,
            state: Arc::new(Mutex::new(WriterState {
                input: DirectionState::new(input_attributes),
                output: DirectionState::new(output_attributes),
                total_content_bytes_remaining: content_bytes,
                fixed_string_bytes: HashMap::new(),
                model: None,
                provider_cost_suppressed: false,
                rate_card_version: None,
            })),
        };
        writer
            .span
            .set_attribute("openinference.span.kind", kind.openinference_value());
        writer
    }

    pub fn instrument<T>(&self, future: T) -> tracing::instrument::Instrumented<T>
    where
        T: Future,
    {
        future.instrument(self.span.clone())
    }

    pub fn enter(&self) -> tracing::span::Entered<'_> {
        self.span.enter()
    }

    pub fn content_enabled(&self) -> bool {
        self.log_content
    }

    pub fn content_bytes_remaining(&self, direction: ContentDirection) -> usize {
        let state = self.state.lock().expect("LLM span writer lock poisoned");
        state
            .direction(direction)
            .budget
            .content_bytes_remaining
            .min(state.total_content_bytes_remaining)
    }

    pub fn can_record_attributes(&self, direction: ContentDirection, count: usize) -> bool {
        self.state
            .lock()
            .expect("LLM span writer lock poisoned")
            .direction(direction)
            .budget
            .can_reserve_attributes(count)
    }

    pub fn try_metadata(
        &self,
        direction: ContentDirection,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> bool {
        self.try_metadata_preserving(direction, key, value, 0, 0)
    }

    pub fn try_metadata_preserving(
        &self,
        direction: ContentDirection,
        key: impl Into<String>,
        value: impl Into<String>,
        reserved_attributes: usize,
        reserved_content_bytes: usize,
    ) -> bool {
        self.try_metadata_group_preserving(
            direction,
            [(key.into(), value.into())],
            reserved_attributes,
            reserved_content_bytes,
        )
    }

    pub fn try_metadata_group<I>(&self, direction: ContentDirection, fields: I) -> bool
    where
        I: IntoIterator<Item = (String, String)>,
    {
        self.try_metadata_group_preserving(direction, fields, 0, 0)
    }

    pub fn try_metadata_group_preserving<I>(
        &self,
        direction: ContentDirection,
        fields: I,
        reserved_attributes: usize,
        reserved_content_bytes: usize,
    ) -> bool
    where
        I: IntoIterator<Item = (String, String)>,
    {
        let fields = fields.into_iter().collect::<Vec<_>>();
        let Some(reservation) = self.reserve_fields(
            direction,
            fields,
            reserved_attributes,
            reserved_content_bytes,
        ) else {
            return false;
        };
        self.write_reserved("", reservation);
        true
    }

    fn reserve_fields(
        &self,
        direction: ContentDirection,
        fields: Vec<(String, String)>,
        reserved_attributes: usize,
        reserved_content_bytes: usize,
    ) -> Option<ReservedLlmAttributes> {
        let bytes = fields
            .iter()
            .map(|(_, value)| value.len())
            .fold(0usize, usize::saturating_add);
        let mut state = self.state.lock().expect("LLM span writer lock poisoned");
        if !state.reserve_dynamic(
            direction,
            fields.len(),
            bytes,
            reserved_attributes,
            reserved_content_bytes,
        ) {
            return None;
        }
        Some(ReservedLlmAttributes { fields })
    }

    pub(super) fn reserve_metadata_group<I>(
        &self,
        direction: ContentDirection,
        fields: I,
    ) -> Option<ReservedLlmAttributes>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        self.reserve_fields(direction, fields.into_iter().collect(), 0, 0)
    }

    pub(super) fn reserve_content(
        &self,
        direction: ContentDirection,
        suffix: String,
        value: String,
    ) -> Option<ReservedLlmAttributes> {
        self.log_content
            .then(|| self.reserve_fields(direction, vec![(suffix, value)], 0, 0))
            .flatten()
    }

    pub(super) fn write_reserved(&self, prefix: &str, reservation: ReservedLlmAttributes) {
        for (suffix, value) in reservation.fields {
            self.span.set_attribute(format!("{prefix}{suffix}"), value);
        }
    }

    pub fn try_content(
        &self,
        direction: ContentDirection,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> bool {
        self.try_content_preserving(direction, key, value, 0, 0)
    }

    pub fn try_content_preserving(
        &self,
        direction: ContentDirection,
        key: impl Into<String>,
        value: impl Into<String>,
        reserved_attributes: usize,
        reserved_content_bytes: usize,
    ) -> bool {
        if !self.log_content {
            return false;
        }
        self.try_metadata_preserving(
            direction,
            key,
            value,
            reserved_attributes,
            reserved_content_bytes,
        )
    }

    pub fn record_json<T: serde::Serialize>(
        &self,
        direction: ContentDirection,
        value: &T,
    ) -> Option<JsonSpanContent> {
        if !self.log_content {
            return None;
        }
        let json = match serde_json::to_string(value) {
            Ok(json) => json,
            Err(_) => {
                self.truncation(
                    direction,
                    "raw_content",
                    serde_json::json!({"reason": "serialization_failed"}),
                );
                return None;
            }
        };
        self.record_json_string(direction, json)
    }

    pub fn record_json_string(
        &self,
        direction: ContentDirection,
        json: String,
    ) -> Option<JsonSpanContent> {
        if !self.reserve_json_content(direction, json.len()) {
            return None;
        }
        Some(set_json_span_content(
            self,
            direction,
            json,
            &self.object_store,
        ))
    }

    /// Check content logging and inline byte budgets before copying raw JSON.
    pub fn record_json_str(
        &self,
        direction: ContentDirection,
        json: &str,
    ) -> Option<JsonSpanContent> {
        if !self.reserve_json_content(direction, json.len()) {
            return None;
        }
        Some(set_json_span_content(
            self,
            direction,
            json.to_owned(),
            &self.object_store,
        ))
    }

    fn reserve_json_content(&self, direction: ContentDirection, bytes: usize) -> bool {
        if !self.log_content {
            return false;
        }
        if bytes <= MAX_INLINE_CONTENT_BYTES {
            let mut state = self.state.lock().expect("LLM span writer lock poisoned");
            if !state.reserve_fixed_content(direction, bytes) {
                drop(state);
                self.truncation(
                    direction,
                    "raw_content",
                    serde_json::json!({"bytes": bytes, "reason": "byte_budget_exhausted"}),
                );
                return false;
            }
        }
        true
    }

    pub fn record_text(&self, direction: ContentDirection, text: impl Into<String>) -> bool {
        if !self.log_content {
            return false;
        }
        let text = text.into();
        let mut state = self.state.lock().expect("LLM span writer lock poisoned");
        if !state.reserve_fixed_content(direction, text.len()) {
            drop(state);
            self.truncation(
                direction,
                "raw_content",
                serde_json::json!({"bytes": text.len(), "reason": "byte_budget_exhausted"}),
            );
            return false;
        }
        drop(state);
        self.span.set_attribute(direction.value_attribute(), text);
        self.span
            .set_attribute(direction.mime_type_attribute(), "text/plain");
        true
    }

    pub fn truncation(&self, direction: ContentDirection, key: impl Into<String>, value: Value) {
        let details = {
            let mut state = self.state.lock().expect("LLM span writer lock poisoned");
            let direction_state = state.direction_mut(direction);
            direction_state.truncation.insert(key.into(), value);
            direction_state.truncation.clone()
        };
        self.span.set_attribute(
            direction.truncation_attribute(),
            Value::Object(details).to_string(),
        );
    }

    fn set_bounded_fixed(
        &self,
        direction: ContentDirection,
        key: &'static str,
        value: impl Into<String>,
        omission_key: &'static str,
    ) -> bool {
        let value = value.into();
        let mut state = self.state.lock().expect("LLM span writer lock poisoned");
        let previous = state.fixed_string_bytes.get(key).copied().unwrap_or(0);
        let available = state
            .total_content_bytes_remaining
            .saturating_add(previous)
            .min(MAX_FIXED_STRING_BYTES);
        if value.len() > MAX_FIXED_STRING_BYTES
            || !state.reserve_bounded_fixed_string(key, value.len())
        {
            drop(state);
            self.truncation(
                direction,
                omission_key,
                serde_json::json!({
                    "bytes": value.len(),
                    "max_bytes": available,
                    "reason": "byte_budget_exhausted",
                }),
            );
            return false;
        }
        drop(state);
        self.span.set_attribute(key, value);
        true
    }

    pub fn set_invocation_parameters_json(&self, json: String) -> bool {
        if json.len() > MAX_INVOCATION_PARAMETERS_BYTES {
            self.truncation(
                ContentDirection::Input,
                "invocation_parameters",
                serde_json::json!({"max_bytes": MAX_INVOCATION_PARAMETERS_BYTES}),
            );
            return false;
        }
        self.set_bounded_fixed(
            ContentDirection::Input,
            "llm.invocation_parameters",
            json,
            "invocation_parameters",
        )
    }

    pub fn set_route(&self, resolution: &crate::backend::RouteResolution) {
        if let Ok(mut state) = self.state.lock() {
            state.model = Some((
                resolution.provider_kind.clone(),
                resolution.upstream_model_id.clone(),
            ));
        }
        self.set_bounded_fixed(
            ContentDirection::Input,
            "llm.system",
            resolution.provider_kind.clone(),
            "route_metadata",
        );
        self.set_bounded_fixed(
            ContentDirection::Input,
            "llm.model_name",
            resolution.upstream_model_id.clone(),
            "model_name",
        );
        self.set_bounded_fixed(
            ContentDirection::Input,
            "llm.provider",
            resolution.provider_kind.clone(),
            "route_metadata",
        );
        if let Some(id) = &resolution.connection_id {
            self.set_bounded_fixed(
                ContentDirection::Input,
                "chalk.connection_id",
                id.clone(),
                "route_metadata",
            );
        }
        self.set_bounded_fixed(
            ContentDirection::Input,
            "chalk.connection_name",
            resolution.connection_name.clone(),
            "route_metadata",
        );
    }

    pub fn set_model_system(&self, model: impl Into<String>, system: impl Into<String>) {
        let model = model.into();
        let system = system.into();
        if let Ok(mut state) = self.state.lock() {
            state.model = Some((system.clone(), model.clone()));
        }
        self.set_bounded_fixed(
            ContentDirection::Input,
            "llm.model_name",
            model,
            "model_name",
        );
        self.set_bounded_fixed(
            ContentDirection::Input,
            "llm.system",
            system,
            "route_metadata",
        );
    }

    /// Mark a router response-cache hit as having zero upstream provider cost.
    ///
    /// The cached response can still stamp the original token counts, but those
    /// counts describe the payload served to the caller rather than a provider
    /// purchase made by this request. Zero is truthful here (unlike an unknown
    /// model) because the router knows it skipped dispatch entirely.
    pub fn suppress_provider_cost_for_cache_hit(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.provider_cost_suppressed = true;
        }
        self.span.set_attribute("llm.cost.total", 0.0);
        self.span.set_attribute("llm.cost.prompt", 0.0);
        self.span.set_attribute("llm.cost.completion", 0.0);
        self.span.set_attribute("chalk.cost.source", "router_cache");
    }

    pub fn set_system(&self, system: impl Into<String>) {
        self.set_bounded_fixed(
            ContentDirection::Input,
            "llm.system",
            system,
            "route_metadata",
        );
    }

    pub fn set_mime_type(&self, direction: ContentDirection, mime_type: impl Into<String>) {
        self.set_bounded_fixed(
            direction,
            direction.mime_type_attribute(),
            mime_type,
            "mime_type",
        );
    }

    pub fn set_embedding_route(&self, model: impl Into<String>, system: impl Into<String>) {
        let model = model.into();
        let system = system.into();
        self.set_bounded_fixed(
            ContentDirection::Input,
            "embedding.model_name",
            model.clone(),
            "model_name",
        );
        self.set_bounded_fixed(
            ContentDirection::Input,
            "embedding.system",
            system.clone(),
            "route_metadata",
        );
        self.set_model_system(model, system);
    }

    pub fn set_stream(&self, streaming: bool) {
        self.span.set_attribute("llm.stream", streaming);
    }

    /// Record the authenticated caller without conflating service/API-key
    /// principals with an end user. The caller passes only server-verified
    /// identity; arbitrary request fields must not become attribution.
    pub fn set_principal(&self, principal_id: Option<&str>, principal_type: Option<&str>) {
        if let (Some(principal_id), Some(principal_type)) = (principal_id, principal_type) {
            self.set_bounded_fixed(
                ContentDirection::Input,
                "chalk.auth.principal.id",
                principal_id,
                "principal_id",
            );
            self.set_bounded_fixed(
                ContentDirection::Input,
                "chalk.auth.principal.type",
                principal_type,
                "principal_type",
            );
        }
    }

    /// Record a trusted end-user identity using OpenInference's standard
    /// attribute. API-key identities must not be promoted to users.
    pub fn set_user_id(&self, user_id: Option<&str>) {
        if let Some(user_id) = user_id {
            self.set_bounded_fixed(ContentDirection::Input, "user.id", user_id, "user_id");
        }
    }

    /// Record a caller-supplied workflow/session grouping id without treating
    /// it as authenticated user attribution.
    pub fn set_session_id(&self, session_id: Option<&str>) {
        if let Some(session_id) = session_id {
            self.set_bounded_fixed(
                ContentDirection::Input,
                "session.id",
                session_id,
                "session_id",
            );
        }
    }

    /// Keep this span's trace regardless of the sampling rate, because of
    /// something the request turned out to be — a moderation check that
    /// flagged it, for example. See [`crate::telemetry::sampling`] for why the
    /// keep decision waits for the span to end rather than living in a sampler.
    pub fn set_sampling_force_keep(&self) {
        self.span
            .set_attribute(crate::telemetry::SAMPLING_FORCE_KEEP_ATTRIBUTE, true);
    }

    /// Override this trace's sampling decision with `rate`, resolved from a
    /// matched [`crate::sampling_policy::OutcomeRule`] against the response
    /// body. Unlike [`Self::set_sampling_force_keep`] this can push the
    /// decision to either keep or drop — see
    /// [`crate::telemetry::sampling::SamplingGate::resolve_outcome`].
    pub fn set_sampling_outcome_rate(&self, rate: f64) {
        self.span
            .set_attribute(crate::telemetry::SAMPLING_OUTCOME_RATE_ATTRIBUTE, rate);
    }

    /// Check `response` against the active sampling policy's outcome rules
    /// and, if one matches, override this trace's sampling decision — the
    /// one call every surface's response-completion point needs. No-op if
    /// none matches (or the policy has none configured).
    pub fn apply_outcome_sampling(
        &self,
        gate: &Arc<crate::telemetry::sampling::SamplingGate>,
        response: &serde_json::Value,
    ) {
        if let Some(rate) = gate.matched_outcome_rate(response) {
            self.set_sampling_outcome_rate(rate);
        }
    }

    /// Evaluate cached Chat selectors against typed metadata, serializing the
    /// response only when an arbitrary selector needs the generic JSON shape.
    pub fn apply_chat_outcome_sampling(
        &self,
        gate: &Arc<crate::telemetry::sampling::SamplingGate>,
        choices: &[crate::types::ChatChoice],
        completion: Option<&crate::types::ChatCompletionResponse>,
    ) {
        if let Some(rate) = gate.matched_chat_outcome_rate(choices, completion) {
            self.set_sampling_outcome_rate(rate);
        }
    }

    pub fn set_requested_served_models(&self, requested: &str, served: &str) {
        if requested == served {
            return;
        }
        self.set_bounded_fixed(
            ContentDirection::Input,
            "chalk.router.requested_model",
            requested.to_string(),
            "route_metadata",
        );
        self.set_bounded_fixed(
            ContentDirection::Input,
            "chalk.router.served_model",
            served.to_string(),
            "route_metadata",
        );
    }

    pub fn set_attempt_summary(&self, attempt_count: usize, fallback_used: bool) {
        self.span
            .set_attribute("chalk.router.attempt_count", attempt_count as i64);
        self.span
            .set_attribute("chalk.router.fallback_used", fallback_used);
    }

    pub fn set_finish_reason(&self, reason: impl Into<String>) {
        self.set_bounded_fixed(
            ContentDirection::Output,
            "llm.finish_reason",
            reason,
            "finish_reason",
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_token_usage(
        &self,
        prompt: u64,
        completion: Option<u64>,
        total: u64,
        cache_read: u64,
        cache_write: u64,
        reasoning: u64,
    ) {
        self.set_token_usage_with_attribution(
            prompt,
            completion,
            total,
            cache_read,
            cache_write,
            reasoning,
            crate::pricing::InputTokenAttribution::TextRate,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn set_token_usage_with_attribution(
        &self,
        prompt: u64,
        completion: Option<u64>,
        total: u64,
        cache_read: u64,
        cache_write: u64,
        reasoning: u64,
        input_token_attribution: crate::pricing::InputTokenAttribution,
    ) {
        self.span
            .set_attribute("llm.token_count.prompt", prompt as i64);
        if let Some(completion) = completion {
            self.span
                .set_attribute("llm.token_count.completion", completion as i64);
        }
        self.span
            .set_attribute("llm.token_count.total", total as i64);
        if cache_read > 0 {
            self.span.set_attribute(
                "llm.token_count.prompt_details.cache_read",
                cache_read as i64,
            );
        }
        if cache_write > 0 {
            self.span.set_attribute(
                "llm.token_count.prompt_details.cache_write",
                cache_write as i64,
            );
        }
        if reasoning > 0 {
            self.span.set_attribute(
                "llm.token_count.completion_details.reasoning",
                reasoning as i64,
            );
        }
        self.set_cost(crate::pricing::TokenUsage {
            prompt,
            completion: completion.unwrap_or(0),
            cache_read,
            cache_write,
            input_token_attribution,
        });
    }

    /// Price the request and record it as `llm.cost.*`.
    ///
    /// Cost is emitted here, beside the counts it is derived from, because this
    /// is the one point both provider paths reach after normalizing their
    /// disagreeing cache conventions -- see `crate::pricing` for what that
    /// normalization means for the arithmetic.
    ///
    /// A model absent from the price table records nothing at all. Zero would be
    /// indistinguishable from a free request, and reintroduces the very bug this
    /// fixes: `llm_cost` reads NULL for "not priced" and 0.0 for "free", and the
    /// evaluation summaries that sum it need to tell those apart.
    fn set_cost(&self, usage: crate::pricing::TokenUsage) {
        let Some((system, model)) = self.state.lock().ok().and_then(|state| {
            (!state.provider_cost_suppressed)
                .then(|| state.model.clone())
                .flatten()
        }) else {
            return;
        };
        let version = self
            .state
            .lock()
            .ok()
            .and_then(|state| state.rate_card_version.clone());
        let card = match version {
            Some(version) => match crate::pricing::card_effective_at(&version)
                .filter(|card| card.effective_from == version)
            {
                Some(card) => card,
                None => return,
            },
            None => crate::pricing::current(),
        };
        let Some(cost) = crate::pricing::cost_usd_with(card, &system, &model, &usage) else {
            tracing::debug!(
                system = %system,
                model = %model,
                "model rates or token billing attribution unavailable; llm.cost.* omitted"
            );
            return;
        };
        // `llm.cost.total` first: it is the key chalk.telemetry.spans coalesces
        // on, and the SDK keeps only the first N attributes when it hits its
        // limit.
        self.span.set_attribute("llm.cost.total", cost.total_usd);
        self.span.set_attribute("llm.cost.prompt", cost.prompt_usd);
        self.span
            .set_attribute("llm.cost.completion", cost.completion_usd);
        self.span.set_attribute("chalk.cost.source", "estimated");
        self.span
            .set_attribute("chalk.cost.rate_card_version", card.effective_from.clone());
    }

    pub(crate) fn set_rate_card_version(&self, version: &str) {
        self.state
            .lock()
            .expect("LLM span writer lock poisoned")
            .rate_card_version = Some(version.to_string());
    }

    pub(crate) fn background_context(&self) -> crate::background_responses::TraceContext {
        use opentelemetry::{baggage::BaggageExt, trace::TraceContextExt};
        let context = self.span.context();
        let current = opentelemetry::Context::current();
        let span = context.span();
        let span_context = span.span_context();
        let keys = [
            crate::telemetry::SAMPLING_DECISION_ATTRIBUTE,
            crate::telemetry::SAMPLING_FORCE_KEEP_ATTRIBUTE,
            crate::telemetry::SAMPLING_LOG_CONTENT_ATTRIBUTE,
            crate::telemetry::SESSION_ID_ATTRIBUTE,
        ];
        crate::background_responses::TraceContext {
            trace_id: span_context.trace_id().to_string(),
            span_id: span_context.span_id().to_string(),
            trace_flags: span_context.trace_flags().to_u8(),
            baggage: keys
                .into_iter()
                .filter_map(|key| {
                    current
                        .baggage()
                        .get(key)
                        .map(|value| (key.to_string(), value.to_string()))
                })
                .collect(),
        }
    }

    pub(crate) fn restore_background_context(
        &self,
        context: &crate::background_responses::TraceContext,
    ) {
        use opentelemetry::trace::{SpanContext, SpanId, TraceFlags, TraceId, TraceState};
        if let (Ok(trace_id), Ok(span_id)) = (
            TraceId::from_hex(&context.trace_id),
            SpanId::from_hex(&context.span_id),
        ) {
            self.span.add_link(SpanContext::new(
                trace_id,
                span_id,
                TraceFlags::new(context.trace_flags),
                true,
                TraceState::default(),
            ));
        }
        for (key, value) in &context.baggage {
            if key == crate::telemetry::SAMPLING_DECISION_ATTRIBUTE
                || key == crate::telemetry::SAMPLING_FORCE_KEEP_ATTRIBUTE
            {
                if let Ok(value) = value.parse::<bool>() {
                    self.span.set_attribute(key.clone(), value);
                }
            } else if key == crate::telemetry::SESSION_ID_ATTRIBUTE {
                self.span.set_attribute(key.clone(), value.clone());
            }
        }
    }

    pub(crate) fn set_background_metrics(&self, attempts: u64, delay_ms: i64) {
        self.span
            .set_attribute("chalk.background.poll_attempts", attempts as i64);
        self.span
            .set_attribute("chalk.background.settlement_delay_ms", delay_ms);
    }

    pub fn set_time_to_first_token_ms(&self, milliseconds: i64) {
        self.span
            .set_attribute("llm.time_to_first_token_ms", milliseconds);
    }

    pub fn set_embedding_input_count(&self, count: usize) {
        self.span
            .set_attribute("embedding.input_count", count as i64);
    }

    pub fn set_embedding_output_shape(&self, count: usize, dimension: Option<usize>) {
        self.span
            .set_attribute("embedding.embeddings_count", count as i64);
        if let Some(dimension) = dimension {
            self.span
                .set_attribute("embedding.vector_dimension", dimension as i64);
        }
    }

    /// Record the exact-prefix cache decision (`chalk.router.cache.*`).
    ///
    /// `age` carries the served entry's age; `None` is a miss. Both are
    /// stamped — a miss is as diagnostic as a hit when someone is asking why
    /// their hit rate is zero. These are short scalars describing the router's
    /// own routing decision, not model content, so they bypass the content
    /// budget the way the other `set_*` routing attributes do.
    pub fn set_cache_outcome(
        &self,
        status: &'static str,
        backend: &'static str,
        scope: &'static str,
        key_hex: &str,
        age: Option<std::time::Duration>,
    ) {
        self.span.set_attribute("chalk.router.cache.status", status);
        self.span
            .set_attribute("chalk.router.cache.backend", backend);
        self.span.set_attribute("chalk.router.cache.scope", scope);
        self.span
            .set_attribute("chalk.router.cache.key", key_hex.to_string());
        if let Some(age) = age {
            self.span
                .set_attribute("chalk.router.cache.age_secs", age.as_secs() as i64);
        }
    }

    pub fn set_route_judge_context(
        &self,
        classification_type: &str,
        default_model: &str,
        default_model_display: &str,
        policy_revision: &str,
        backend: &str,
        shadow: bool,
    ) {
        self.set_bounded_fixed(
            ContentDirection::Output,
            "chalk.router.judge.classification_type",
            classification_type,
            "judge_classification_type",
        );
        self.set_bounded_fixed(
            ContentDirection::Output,
            "chalk.router.judge.default_model",
            default_model,
            "judge_default_model",
        );
        self.set_bounded_fixed(
            ContentDirection::Output,
            "chalk.router.judge.default_model_display",
            default_model_display,
            "judge_default_model_display",
        );
        self.set_bounded_fixed(
            ContentDirection::Output,
            "chalk.router.judge.policy_revision",
            policy_revision,
            "judge_policy_revision",
        );
        self.span
            .set_attribute("chalk.router.judge.backend", backend.to_string());
        self.span.set_attribute("chalk.router.judge.shadow", shadow);
    }

    pub fn set_route_judge_selected(
        &self,
        label: &str,
        model: &str,
        model_display: Option<&str>,
        fallbacks: &[String],
        latency: Duration,
        cache_hit: bool,
    ) {
        self.span
            .set_attribute("chalk.router.judge.status", "selected");
        self.set_bounded_fixed(
            ContentDirection::Output,
            "chalk.router.judge.classification",
            label,
            "judge_classification",
        );
        self.set_bounded_fixed(
            ContentDirection::Output,
            "chalk.router.judge.selected_model",
            model,
            "judge_selected_model",
        );
        if let Some(model_display) = model_display {
            self.set_bounded_fixed(
                ContentDirection::Output,
                "chalk.router.judge.selected_model_display",
                model_display,
                "judge_selected_model_display",
            );
        }
        self.set_bounded_fixed(
            ContentDirection::Output,
            "chalk.router.judge.fallbacks",
            serde_json::to_string(fallbacks).unwrap_or_default(),
            "judge_fallbacks",
        );
        self.set_route_judge_execution(latency, Some(cache_hit));
    }

    pub fn set_route_judge_skipped(&self, reason: &'static str, latency: Duration) {
        self.span
            .set_attribute("chalk.router.judge.status", "skipped");
        self.span.set_attribute("chalk.router.judge.reason", reason);
        self.set_route_judge_execution(latency, None);
    }

    pub fn set_route_judge_failed(&self, reason: &'static str, latency: Duration) {
        self.span
            .set_attribute("chalk.router.judge.status", "failed");
        self.span.set_attribute("chalk.router.judge.reason", reason);
        self.set_route_judge_execution(latency, None);
    }

    fn set_route_judge_execution(&self, latency: Duration, cache_hit: Option<bool>) {
        self.span.set_attribute(
            "chalk.router.judge.latency_ms",
            i64::try_from(latency.as_millis()).unwrap_or(i64::MAX),
        );
        if let Some(cache_hit) = cache_hit {
            self.span
                .set_attribute("chalk.router.judge.cache_hit", cache_hit);
        }
    }

    /// Marking a span as an error also force-keeps its trace. The HTTP-layer
    /// force-keep in `telemetry.rs` only inspects the outer response status,
    /// so it never sees an error that reaches the client in-band inside an
    /// already-200 SSE stream; every handler that stamps a stream error
    /// through this method gets force-keep for free.
    pub fn set_error(&self, error_type: impl Into<String>, message: Option<&str>) {
        self.set_bounded_fixed(
            ContentDirection::Output,
            "error.type",
            error_type,
            "error_type",
        );
        if let Some(message) = message.filter(|_| self.log_content) {
            self.set_bounded_fixed(
                ContentDirection::Output,
                "error.message",
                message.to_string(),
                "error_message",
            );
        }
        self.set_sampling_force_keep();
    }

    pub fn set_error_status(&self, message: impl Into<String>) {
        let message = if self.log_content {
            message.into()
        } else {
            "request failed".to_string()
        };
        self.span
            .set_status(opentelemetry::trace::Status::error(message));
    }
}

pub struct JsonSpanContent {
    #[allow(dead_code)]
    pub inline_bytes: usize,
    #[allow(dead_code)]
    pub upload: Option<JoinHandle<()>>,
}

/// Store small JSON values inline. Oversized values are uploaded by a bounded
/// task with a total timeout and one retry. The upload reuses one shared buffer
/// across attempts, but is deliberately accounted as a fully resident payload:
/// callers serialize the complete JSON before this function, so an HTTP body
/// stream alone would not lower peak memory. No span clone or payload is queued
/// when admission fails.
fn set_json_span_content(
    span: &LlmSpanWriter,
    direction: ContentDirection,
    json: String,
    object_store: &storage::SharedObjectStore,
) -> JsonSpanContent {
    if json.len() <= MAX_INLINE_CONTENT_BYTES {
        return set_json_span_content_with_memory(span, direction, json, object_store, None);
    }
    set_json_span_content_with_memory(span, direction, json, object_store, pod_memory())
}

fn set_json_span_content_with_memory(
    span: &LlmSpanWriter,
    direction: ContentDirection,
    json: String,
    object_store: &storage::SharedObjectStore,
    pod_memory: Option<PodMemory>,
) -> JsonSpanContent {
    if json.len() <= MAX_INLINE_CONTENT_BYTES {
        let inline_bytes = json.len();
        span.span.set_attribute(direction.value_attribute(), json);
        span.span
            .set_attribute(direction.mime_type_attribute(), "application/json");
        return JsonSpanContent {
            inline_bytes,
            upload: None,
        };
    }

    span.span.set_attribute(
        direction.content_bytes_attribute(),
        i64::try_from(json.len()).unwrap_or(i64::MAX),
    );

    let Some(store) = object_store.clone() else {
        span.span.set_attribute(
            direction.upload_omitted_attribute(),
            "object_storage_disabled",
        );
        return JsonSpanContent {
            inline_bytes: 0,
            upload: None,
        };
    };

    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        span.span
            .set_attribute(direction.upload_omitted_attribute(), "runtime_unavailable");
        tracing::warn!(
            content = direction.label(),
            content_bytes = json.len(),
            reason = "runtime_unavailable",
            "Dropping oversized trace content upload"
        );
        return JsonSpanContent {
            inline_bytes: 0,
            upload: None,
        };
    };

    let memory_limit = content_upload_memory_limit(pod_memory);
    let Some(memory_reservation) = try_reserve_upload_memory(json.len(), memory_limit) else {
        span.span.set_attribute(
            direction.upload_omitted_attribute(),
            "upload_memory_budget_exceeded",
        );
        tracing::warn!(
            content = direction.label(),
            content_bytes = json.len(),
            upload_memory_bytes = CONTENT_UPLOAD_BYTES.load(Ordering::Acquire),
            upload_memory_limit_bytes = memory_limit,
            reason = "upload_memory_budget_exceeded",
            "Dropping oversized trace content upload"
        );
        return JsonSpanContent {
            inline_bytes: 0,
            upload: None,
        };
    };

    let Ok(permit) = CONTENT_UPLOAD_PERMITS.clone().try_acquire_owned() else {
        span.span
            .set_attribute(direction.upload_omitted_attribute(), "upload_queue_full");
        tracing::warn!(
            content = direction.label(),
            content_bytes = json.len(),
            reason = "upload_queue_full",
            "Dropping oversized trace content upload"
        );
        return JsonSpanContent {
            inline_bytes: 0,
            upload: None,
        };
    };

    let span = span.clone();
    let upload = runtime.spawn(async move {
        let json = bytes::Bytes::from(json.into_bytes());
        let result = tokio::time::timeout(CONTENT_UPLOAD_TIMEOUT, async {
            let mut last_error = String::new();
            for attempt in 0..CONTENT_UPLOAD_ATTEMPTS {
                match store.put_hashed_bytes(json.clone(), "json").await {
                    Ok(key) => return Ok(key),
                    Err(error) => last_error = error.to_string(),
                }
                if attempt + 1 < CONTENT_UPLOAD_ATTEMPTS {
                    tokio::task::yield_now().await;
                }
            }
            Err(last_error)
        })
        .await;
        drop(permit);
        drop(memory_reservation);

        match result {
            Ok(Ok(key)) => {
                span.set_bounded_fixed(
                    direction,
                    direction.content_uri_attribute(),
                    store.uri_for_key(&key),
                    "raw_content",
                );
            }
            Ok(Err(error)) => {
                span.span
                    .set_attribute(direction.upload_omitted_attribute(), "upload_failed");
                tracing::warn!(
                    error = %error,
                    content = direction.label(),
                    "Failed to upload oversized trace content"
                );
            }
            Err(_) => {
                span.span
                    .set_attribute(direction.upload_omitted_attribute(), "upload_timed_out");
                tracing::warn!(
                    timeout_ms = CONTENT_UPLOAD_TIMEOUT.as_millis() as i64,
                    content = direction.label(),
                    "Timed out uploading oversized trace content"
                );
            }
        }
    });

    JsonSpanContent {
        inline_bytes: 0,
        upload: Some(upload),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
    use opentelemetry_sdk::trace::{SimpleSpanProcessor, TracerProvider};
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn metadata_only_errors_do_not_export_provider_echoes() {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("private-error")));
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let writer = LlmSpanWriter::new_with_limits(
                tracing::info_span!("private_error"),
                LlmSpanKind::Llm,
                10,
                10,
                1024,
                false,
            );
            writer.set_error("upstream_error", Some("secret-prompt-echo"));
            writer.set_error_status("secret-response-echo");
        }
        let attrs = exported_attributes(&provider, &exporter, "private_error");
        assert_eq!(
            attrs.get("error.type").map(String::as_str),
            Some("upstream_error")
        );
        assert!(!attrs.contains_key("error.message"));
        let spans = exporter.get_finished_spans().unwrap();
        assert!(!format!("{spans:?}").contains("secret-"));
    }

    #[test]
    fn budgets_are_derived_from_the_configured_limit() {
        let (input, output) = llm_attribute_budgets();
        assert_eq!(
            input + output,
            crate::telemetry::span_attribute_count_limit()
                .saturating_sub(LLM_FIXED_ATTRIBUTE_SLOTS.len() + LLM_FIXED_ATTRIBUTE_HEADROOM)
        );
        assert!(output > input);
    }

    #[test]
    fn metadata_strings_consume_the_content_budget_atomically() {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer().with_tracer(provider.tracer("atomic-budget-test")),
        );
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let span = tracing::info_span!("atomic_budget");
            let writer = LlmSpanWriter::new_with_limits(span, LlmSpanKind::Llm, 0, 3, 10, true);
            assert!(!writer.try_metadata_group(
                ContentDirection::Output,
                [
                    ("failed.0".to_string(), "123456".to_string()),
                    ("failed.1".to_string(), "12345".to_string())
                ],
            ));
            assert!(writer.try_metadata_group(
                ContentDirection::Output,
                [
                    ("kept.0".to_string(), "12345".to_string()),
                    ("kept.1".to_string(), "67890".to_string())
                ],
            ));
            assert!(writer.try_metadata(ContentDirection::Output, "kept.2", ""));
        }
        let attributes = exported_attributes(&provider, &exporter, "atomic_budget");
        assert!(!attributes.contains_key("failed.0"));
        assert_eq!(attributes.get("kept.0"), Some(&"12345".to_string()));
        assert!(attributes.contains_key("kept.2"));
    }

    #[test]
    fn remaining_bytes_are_clamped_to_the_shared_pool() {
        let span = tracing::info_span!("shared_byte_budget");
        let writer = LlmSpanWriter::new_with_limits(span, LlmSpanKind::Llm, 2, 2, 10, true);
        assert_eq!(writer.content_bytes_remaining(ContentDirection::Input), 10);
        assert_eq!(writer.content_bytes_remaining(ContentDirection::Output), 10);

        assert!(writer.try_metadata(ContentDirection::Input, "input.field", "123456"));
        assert_eq!(writer.content_bytes_remaining(ContentDirection::Input), 4);
        assert_eq!(writer.content_bytes_remaining(ContentDirection::Output), 4);

        assert!(writer.try_metadata(ContentDirection::Output, "output.field", "7890"));
        assert_eq!(writer.content_bytes_remaining(ContentDirection::Input), 0);
        assert_eq!(writer.content_bytes_remaining(ContentDirection::Output), 0);
    }

    #[test]
    fn metadata_only_writer_cannot_emit_content() {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer()
                .with_tracer(provider.tracer("metadata-only-writer-test")),
        );
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let span = tracing::info_span!("metadata_only_writer");
            let writer = LlmSpanWriter::new_with_limits(span, LlmSpanKind::Llm, 4, 4, 128, false);
            assert!(writer.try_metadata(
                ContentDirection::Input,
                "llm.input_messages.0.message.role",
                "user",
            ));
            assert!(!writer.try_content(
                ContentDirection::Input,
                "llm.input_messages.0.message.content",
                "secret",
            ));
            assert!(writer
                .record_json(
                    ContentDirection::Input,
                    &serde_json::json!({"secret": true})
                )
                .is_none());
            assert!(writer
                .record_json_str(ContentDirection::Output, r#"{"secret":true}"#)
                .is_none());
        }
        let attributes = exported_attributes(&provider, &exporter, "metadata_only_writer");
        assert_eq!(
            attributes.get("llm.input_messages.0.message.role"),
            Some(&"user".to_string())
        );
        assert!(!attributes.contains_key("llm.input_messages.0.message.content"));
        assert!(!attributes.contains_key("input.value"));
        assert!(!attributes.contains_key("output.value"));
    }

    #[test]
    fn borrowed_json_preserves_wire_content_and_obeys_the_inline_budget() {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer().with_tracer(provider.tracer("borrowed-json-test")),
        );
        let json = r#"{ "number":1.0000000000000001, "escaped":"\u0061", "null":null }"#;
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let writer = LlmSpanWriter::new_with_limits(
                tracing::info_span!("borrowed_json"),
                LlmSpanKind::Llm,
                4,
                4,
                json.len(),
                true,
            );
            assert!(writer
                .record_json_str(ContentDirection::Output, json)
                .is_some());
            assert!(writer
                .record_json_str(ContentDirection::Output, "{}")
                .is_none());
        }
        let attributes = exported_attributes(&provider, &exporter, "borrowed_json");
        assert_eq!(attributes.get("output.value"), Some(&json.to_string()));
        assert!(attributes["chalk.trace.output.truncation"].contains("byte_budget_exhausted"));
    }

    #[test]
    fn set_error_force_keeps_the_trace() {
        // A stream error that reaches the client in-band (200 status, an
        // `event: error` frame) is otherwise invisible to the HTTP-layer
        // force-keep in `telemetry.rs`, which only inspects the outer
        // response status. `set_error` is the one place every such handler
        // marks a span as errored, so force-keeping there is what makes those
        // traces survive a low sampling rate.
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("set-error-test")));
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let span = tracing::info_span!("set_error_span");
            let writer = LlmSpanWriter::new_with_limits(span, LlmSpanKind::Llm, 4, 4, 128, true);
            writer.set_error("stream_transport_error", Some("boom"));
        }
        let attributes = exported_attributes(&provider, &exporter, "set_error_span");
        assert_eq!(
            attributes.get(crate::telemetry::SAMPLING_FORCE_KEEP_ATTRIBUTE),
            Some(&"true".to_string()),
            "marking a span as an error must force-keep its trace"
        );
    }

    #[test]
    fn set_sampling_outcome_rate_stamps_the_attribute() {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("outcome-rate-test")));
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let span = tracing::info_span!("outcome_rate_span");
            let writer = LlmSpanWriter::new_with_limits(span, LlmSpanKind::Llm, 4, 4, 128, true);
            writer.set_sampling_outcome_rate(0.1);
        }
        let attributes = exported_attributes(&provider, &exporter, "outcome_rate_span");
        assert_eq!(
            attributes.get(crate::telemetry::SAMPLING_OUTCOME_RATE_ATTRIBUTE),
            Some(&"0.1".to_string())
        );
    }

    #[test]
    fn route_judge_attributes_form_a_complete_shadow_decision() {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("route-judge-test")));
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let span = tracing::info_span!("route_judge_span");
            let writer = LlmSpanWriter::new_with_limits(span, LlmSpanKind::Llm, 4, 4, 512, true);
            writer.set_route_judge_context(
                "task",
                "default/model",
                "Default Connection/model",
                "sha256:policy",
                "completion",
                true,
            );
            writer.set_route_judge_selected(
                "coding",
                "selected/model",
                Some("OpenAI/gpt-5.5"),
                &["fallback/model".into()],
                Duration::from_millis(17),
                true,
            );
        }
        let attributes = exported_attributes(&provider, &exporter, "route_judge_span");
        assert_eq!(
            attributes.get("chalk.router.judge.status"),
            Some(&"selected".to_string())
        );
        assert_eq!(
            attributes.get("chalk.router.judge.classification"),
            Some(&"coding".to_string())
        );
        assert_eq!(
            attributes.get("chalk.router.judge.selected_model"),
            Some(&"selected/model".to_string())
        );
        assert_eq!(
            attributes.get("chalk.router.judge.selected_model_display"),
            Some(&"OpenAI/gpt-5.5".to_string())
        );
        assert_eq!(
            attributes.get("chalk.router.judge.fallbacks"),
            Some(&"[\"fallback/model\"]".to_string())
        );
        assert_eq!(
            attributes.get("chalk.router.judge.shadow"),
            Some(&"true".to_string())
        );
        assert_eq!(
            attributes.get("chalk.router.judge.classification_type"),
            Some(&"task".to_string())
        );
        assert_eq!(
            attributes.get("chalk.router.judge.default_model"),
            Some(&"default/model".to_string())
        );
        assert_eq!(
            attributes.get("chalk.router.judge.default_model_display"),
            Some(&"Default Connection/model".to_string())
        );
        assert_eq!(
            attributes.get("chalk.router.judge.policy_revision"),
            Some(&"sha256:policy".to_string())
        );
        assert_eq!(
            attributes.get("chalk.router.judge.backend"),
            Some(&"completion".to_string())
        );
        assert_eq!(
            attributes.get("chalk.router.judge.latency_ms"),
            Some(&"17".to_string())
        );
        assert_eq!(
            attributes.get("chalk.router.judge.cache_hit"),
            Some(&"true".to_string())
        );
        assert!(!attributes.contains_key("chalk.router.judge.reason"));
    }

    #[test]
    fn route_judge_skips_and_failures_are_queryable() {
        for (status, reason, failed) in [
            ("skipped", "timeout", false),
            ("failed", "classification_error", true),
        ] {
            let exporter = InMemorySpanExporter::default();
            let provider = TracerProvider::builder()
                .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
                .build();
            let subscriber = tracing_subscriber::registry().with(
                tracing_opentelemetry::layer().with_tracer(provider.tracer("route-judge-test")),
            );
            {
                let _guard = tracing::subscriber::set_default(subscriber);
                let span = tracing::info_span!("route_judge_outcome");
                let writer =
                    LlmSpanWriter::new_with_limits(span, LlmSpanKind::Llm, 4, 4, 256, true);
                writer.set_route_judge_context(
                    "difficulty",
                    "default/model",
                    "Default Connection/model",
                    "sha256:policy",
                    "decisions",
                    true,
                );
                if failed {
                    writer.set_route_judge_failed(reason, Duration::from_millis(9));
                } else {
                    writer.set_route_judge_skipped(reason, Duration::from_millis(9));
                }
            }
            let attributes = exported_attributes(&provider, &exporter, "route_judge_outcome");
            assert_eq!(
                attributes.get("chalk.router.judge.status"),
                Some(&status.to_string())
            );
            assert_eq!(
                attributes.get("chalk.router.judge.reason"),
                Some(&reason.to_string())
            );
            assert_eq!(
                attributes.get("chalk.router.judge.latency_ms"),
                Some(&"9".to_string())
            );
            assert!(!attributes.contains_key("chalk.router.judge.classification"));
            assert!(!attributes.contains_key("chalk.router.judge.cache_hit"));
        }
    }

    #[test]
    fn oversized_fixed_strings_report_their_omission() {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer().with_tracer(provider.tracer("fixed-string-budget-test")),
        );
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let span = tracing::info_span!("fixed_string_budget");
            let writer = LlmSpanWriter::new_with_limits(
                span,
                LlmSpanKind::Llm,
                0,
                0,
                MAX_LLM_SPAN_CONTENT_BYTES,
                true,
            );
            writer.set_model_system("m".repeat(MAX_FIXED_STRING_BYTES + 1), "openai");
        }
        let attributes = exported_attributes(&provider, &exporter, "fixed_string_budget");
        assert!(!attributes.contains_key("llm.model_name"));
        let truncation: Value = serde_json::from_str(
            attributes
                .get("chalk.trace.input.truncation")
                .expect("fixed string omission is reported"),
        )
        .unwrap();
        assert_eq!(
            truncation["model_name"]["bytes"],
            MAX_FIXED_STRING_BYTES + 1
        );
    }

    #[test]
    fn truncation_is_exported_eagerly_and_accumulates() {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer().with_tracer(provider.tracer("eager-truncation-test")),
        );
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let span = tracing::info_span!("eager_truncation");
            let writer = LlmSpanWriter::new_with_limits(span, LlmSpanKind::Llm, 0, 0, 0, true);
            writer.truncation(
                ContentDirection::Output,
                "messages",
                serde_json::json!({"omitted": 1}),
            );
            writer.truncation(
                ContentDirection::Output,
                "tools",
                serde_json::json!({"omitted": 2}),
            );
            // Dropping without a finalization call models an early return.
        }
        let attributes = exported_attributes(&provider, &exporter, "eager_truncation");
        let truncation: Value = serde_json::from_str(
            attributes
                .get("chalk.trace.output.truncation")
                .expect("truncation should be exported before span drop"),
        )
        .unwrap();
        assert_eq!(truncation["messages"]["omitted"], 1);
        assert_eq!(truncation["tools"]["omitted"], 2);
    }

    #[test]
    fn llm_handlers_cannot_bypass_the_typed_writer() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for entry in std::fs::read_dir(manifest.join("src/handlers")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("rs")
                || path.file_name().and_then(|name| name.to_str()) == Some("trace_attributes.rs")
            {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            assert!(
                !source.contains("set_attribute("),
                "{} bypasses LlmSpanWriter",
                path.display(),
            );
        }
        let judge = std::fs::read_to_string(manifest.join("src/judge/routing.rs")).unwrap();
        assert!(!judge.contains("set_attribute("));
    }

    #[test]
    fn upload_memory_limit_uses_the_pod_limit_not_node_memory() {
        let mib = 1024 * 1024;
        assert_eq!(
            content_upload_memory_limit(Some(PodMemory { limit: 512 * mib })),
            16 * mib as usize
        );
        assert_eq!(
            content_upload_memory_limit(Some(PodMemory {
                limit: 8 * 1024 * mib
            })),
            MAX_CONTENT_UPLOAD_MEMORY_BYTES
        );
        assert_eq!(content_upload_memory_limit(None), 128 * mib as usize);
    }

    #[tokio::test]
    async fn payload_larger_than_the_pod_derived_budget_is_not_queued() {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer().with_tracer(provider.tracer("memory-budget-test")),
        );
        let object_store = Some(Arc::new(storage::ObjectStore::memory()));

        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let span = tracing::info_span!("memory_budget_drop");
            let span = LlmSpanWriter::new(span, object_store.clone(), LlmSpanKind::Llm);
            let content = set_json_span_content_with_memory(
                &span,
                ContentDirection::Output,
                "x".repeat(MAX_INLINE_CONTENT_BYTES + 2),
                &object_store,
                Some(PodMemory { limit: 1024 * 1024 }),
            );
            assert!(content.upload.is_none());
            drop(span);
        }

        let attributes = exported_attributes(&provider, &exporter, "memory_budget_drop");
        assert_eq!(
            attributes.get("chalk.trace.output.upload_omitted"),
            Some(&"upload_memory_budget_exceeded".to_string())
        );
    }

    #[test]
    fn missing_runtime_drops_upload_instead_of_panicking() {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("runtime-drop-test")));
        let object_store = Some(Arc::new(storage::ObjectStore::memory()));

        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let span = tracing::info_span!("runtime_unavailable_drop");
            let span = LlmSpanWriter::new(span, object_store.clone(), LlmSpanKind::Llm);
            let content = set_json_span_content_with_memory(
                &span,
                ContentDirection::Output,
                "x".repeat(MAX_INLINE_CONTENT_BYTES + 1),
                &object_store,
                None,
            );
            assert!(content.upload.is_none());
            drop(span);
        }

        let attributes = exported_attributes(&provider, &exporter, "runtime_unavailable_drop");
        assert_eq!(
            attributes.get("chalk.trace.output.upload_omitted"),
            Some(&"runtime_unavailable".to_string())
        );
    }

    #[test]
    fn resolved_route_populates_estimated_cost_metadata() {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("route-cost-test")));
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let span = tracing::info_span!("resolved_route_cost");
            let writer = LlmSpanWriter::new(span, None, LlmSpanKind::Llm);
            writer.set_route(&crate::backend::RouteResolution {
                provider_kind: "anthropic".to_string(),
                connection_id: Some("connection-1".to_string()),
                connection_name: "Anthropic".to_string(),
                upstream_model_id: "claude-sonnet-4-5".to_string(),
                connection_prefix: None,
                model_route_id: None,
            });
            writer.set_token_usage(10, Some(5), 15, 0, 0, 0);
        }

        let attributes = exported_attributes(&provider, &exporter, "resolved_route_cost");
        assert!(attributes.contains_key("llm.cost.total"));
        assert!(attributes.contains_key("llm.cost.prompt"));
        assert!(attributes.contains_key("llm.cost.completion"));
        assert_eq!(
            attributes.get("chalk.cost.source"),
            Some(&"estimated".to_string())
        );
        assert_eq!(
            attributes.get("chalk.cost.rate_card_version"),
            Some(&crate::pricing::current().effective_from)
        );
    }

    #[test]
    fn router_cache_hit_cost_stays_zero_when_tokens_are_stamped() {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("cache-cost-test")));
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            let span = tracing::info_span!("router_cache_cost");
            let writer = LlmSpanWriter::new(span, None, LlmSpanKind::Llm);
            writer.set_route(&crate::backend::RouteResolution {
                provider_kind: "anthropic".to_string(),
                connection_id: Some("connection-1".to_string()),
                connection_name: "Anthropic".to_string(),
                upstream_model_id: "claude-sonnet-4-5".to_string(),
                connection_prefix: None,
                model_route_id: None,
            });
            writer.suppress_provider_cost_for_cache_hit();
            writer.set_token_usage(10, Some(5), 15, 0, 0, 0);
        }

        let attributes = exported_attributes(&provider, &exporter, "router_cache_cost");
        assert_eq!(attributes.get("llm.cost.total"), Some(&"0".to_string()));
        assert_eq!(attributes.get("llm.cost.prompt"), Some(&"0".to_string()));
        assert_eq!(
            attributes.get("llm.cost.completion"),
            Some(&"0".to_string())
        );
        assert_eq!(
            attributes.get("chalk.cost.source"),
            Some(&"router_cache".to_string())
        );
        assert!(!attributes.contains_key("chalk.cost.rate_card_version"));
    }

    fn exported_attributes(
        provider: &TracerProvider,
        exporter: &InMemorySpanExporter,
        span_name: &str,
    ) -> std::collections::HashMap<String, String> {
        for result in provider.force_flush() {
            result.expect("span should flush");
        }
        exporter
            .get_finished_spans()
            .expect("finished spans should be readable")
            .into_iter()
            .find(|span| span.name.as_ref() == span_name)
            .expect("test span should be exported")
            .attributes
            .into_iter()
            .map(|attribute| (attribute.key.to_string(), attribute.value.to_string()))
            .collect()
    }
}
