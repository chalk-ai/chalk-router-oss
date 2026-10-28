//! Outcome-aware trace sampling for the router's LLM frontends.
//!
//! OpenTelemetry's [`ShouldSample`] hook fires exactly once, at span creation,
//! and cannot see the request behind the span: the resolved API key's own rate
//! and the caller's request tag are not on the span builder, and a moderation
//! verdict, an upstream error or a fallback is not known until the request has
//! been served. Those are precisely the traces an operator wants kept at a rate
//! different from the rest of the traffic. So for the LLM frontends the real
//! keep decision is moved off the sampler and onto the export path:
//!
//! 1. [`FrontendRouteSampler`] records every frontend span
//!    ([`opentelemetry_sdk::trace::Sampler::AlwaysOn`]). Recording is
//!    in-process and costs no network traffic; every other route keeps the
//!    static `OTEL_TRACES_SAMPLER` behavior it has today.
//! 2. [`crate::telemetry::OtelMakeSpan`] resolves the rate-based half of the
//!    decision at request entry, where the session or trace id, the request tag
//!    and the per-key rate are all in hand at once, and publishes it as baggage
//!    so every span of the trace carries it.
//! 3. [`SamplingGateSpanProcessor`] sits in front of the OTLP batch processor
//!    and reads that one decision off whichever span it is handed at `on_end`,
//!    plus the outcome-based force-keeps a handler can only discover while
//!    serving the request. This is the same wrapping trick
//!    [`crate::telemetry::braintrust::BraintrustSpanProcessor`] uses to
//!    forward only a subset of spans to its exporter.
//!
//! Deciding a trace once, up front, rather than testing each ending span for
//! LLM markers, is what keeps a dropped trace whole. A frontend request opens
//! descendant spans that carry no marker of their own — the
//! `policy_cache_refresh` [`crate::fallback`] and [`crate::judge_policy`] open
//! when their cached policy has gone stale mid-request — and a marker-based
//! test exports those at full volume, orphaned from a request span that was
//! dropped.
//!
//! The decision is a deterministic hash, never an RNG draw
//! ([`sampled_by_hash`]). Two properties follow, and both are the point:
//!
//! * **One conversation, one decision.** A conversation spans many HTTP
//!   requests (see [`crate::telemetry::SessionIdSpanProcessor`]); hashing the
//!   `session.id` gives every request in it the same answer, instead of
//!   scattering a coin flip across the turns and leaving an operator with a
//!   conversation they can only half read.
//! * **One trace, one decision.** With no session id the key is the trace id,
//!   which every span of a trace shares — equivalent to
//!   `TraceIdRatioBased`, and still reproducible across replicas and replays
//!   because no per-process RNG state is involved.
//!
//! Both keys — `session.id` and, when a caller supplies its own `traceparent`,
//! the trace id itself — are entirely caller-controlled, so the hash is keyed
//! by the environment's own secret ([`SamplingGate::set_sampling_secret`]).
//! Without that, a caller could choose whichever key value hashes to "drop"
//! at the configured rate and make its own traffic permanently unsampled.
//! The secret is shared, not per-process, so "one conversation, one decision"
//! still holds across replicas.
//!
//! The resolved decision, and the request-scoped inputs it was resolved from
//! (the caller's request tag, the key's own rate, the force-trace header),
//! travel as baggage and are stamped on every span at its start — see
//! [`with_sampling_inputs`] and [`SamplingContextSpanProcessor`]. Baggage is
//! the only channel that reaches a span created deep inside a handler.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use hmac::{Hmac, Mac};
use opentelemetry::trace::{SamplingResult, SpanId, TraceId, TraceResult};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::export::trace::SpanData;
use opentelemetry_sdk::trace::{Sampler, ShouldSample, Span, SpanProcessor};
use opentelemetry_sdk::Resource;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

use crate::sampling_policy::{
    CompiledChatOutcomeRules, MatchContext, ResolvedSampling, SamplingPolicy, SamplingPolicyStore,
    FAIL_OPEN_RATE,
};

use super::{
    frontend_for_path, CALLER_REQUEST_TAG_ATTRIBUTE, SAMPLING_DECISION_ATTRIBUTE,
    SAMPLING_FORCE_KEEP_ATTRIBUTE, SAMPLING_LOG_CONTENT_ATTRIBUTE, SAMPLING_OUTCOME_RATE_ATTRIBUTE,
    SESSION_ID_ATTRIBUTE,
};

/// How long a policy snapshot is served before a refresh is triggered. Short
/// enough that an operator's edit takes effect while they are still looking at
/// the UI; long enough that a burst of ending spans costs one query, not one
/// query per span.
const POLICY_SNAPSHOT_TTL: Duration = Duration::from_secs(5);

/// How long a force-kept trace id is remembered. A trace's spans all end within
/// one request's lifetime plus the batch delay, so this only has to outlive a
/// slow streaming response.
const FORCE_KEEP_TTL: Duration = Duration::from_secs(300);

/// Cap on remembered force-kept trace ids. Debug headers and flagged outcomes
/// are rare, so this is a guard against a client that sets the force header on
/// every request, not a working-set estimate.
const MAX_FORCE_KEPT_TRACES: usize = 8_192;

/// Hard cap on completed outcome history. Live frontend traces retain their
/// own decision separately, regardless of eviction/TTL in this FIFO history.
const MAX_OUTCOME_OVERRIDES: usize = 8_192;

/// The policy's answers for one request, hashed down to a keep/drop.
///
/// Both travel to every span of the request as baggage, but only `keep` becomes
/// a span attribute: `log_content` is consumed by handler code as it writes the
/// span, and never needs to reach the export path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestSampling {
    pub keep: bool,
    pub log_content: bool,
}

/// Map a stable hash of `key`, keyed by `secret`, into `[0, 1)` and compare it
/// against `rate`.
///
/// HMAC-SHA256 rather than a bare hash or [`std::hash`]: `key` is entirely
/// caller-controlled (see the module doc), so an unkeyed hash — even a stable
/// one — would let a caller pick whichever key hashes to the side of `rate`
/// it wants. `RandomState` is out for the ordinary reason too: it is seeded
/// per process, so two replicas handling two turns of the same conversation
/// would disagree, and even a fixed-seed `DefaultHasher` is only "stable in
/// practice" — its output may change between Rust releases, which would
/// silently reshuffle which conversations are kept on every toolchain bump.
///
/// A non-finite `rate` keeps the span: an unreadable rate must never be the
/// reason a trace is missing.
pub fn sampled_by_hash(key: &str, rate: f64, secret: &[u8]) -> bool {
    if !rate.is_finite() {
        return true;
    }
    if rate <= 0.0 {
        return false;
    }
    if rate >= 1.0 {
        return true;
    }
    unit_interval(key, secret) < rate
}

/// The first 53 bits of `HMAC-SHA256(secret, key)` as an `f64` in `[0, 1)` —
/// the same construction a uniform-float RNG uses, so the comparison in
/// [`sampled_by_hash`] keeps a `rate` fraction of distinct keys.
fn unit_interval(key: &str, secret: &[u8]) -> f64 {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts a key of any length");
    mac.update(key.as_bytes());
    let digest = mac.finalize().into_bytes();
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    let bits = u64::from_be_bytes(head);
    // 2^-53 exactly; `>> 11` leaves the 53 bits an f64 can hold without loss.
    (bits >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
}

/// Last successful policy read plus the time of the last *attempt*.
///
/// The two are tracked separately so a failing store is retried at most once
/// per TTL window (rather than once per ending span) while the previous
/// known-good policy keeps being served — the same stale-while-error rule
/// [`crate::judge_policy::CachedJudgePolicyStore`] follows, for the same
/// reason: inventing a default policy here would start dropping traces on the
/// strength of a database outage.
struct CachedSamplingPolicy {
    policy: SamplingPolicy,
    chat_outcomes: CompiledChatOutcomeRules,
}

impl std::ops::Deref for CachedSamplingPolicy {
    type Target = SamplingPolicy;

    fn deref(&self) -> &Self::Target {
        &self.policy
    }
}

struct PolicySnapshot {
    policy: Option<Arc<CachedSamplingPolicy>>,
    last_attempt: Instant,
}

/// Shared state behind the sampling gate: the policy snapshot both the request
/// path and the span processor read synchronously, and the set of traces pinned
/// by a force-keep.
///
/// The store arrives *after* telemetry is initialized. `init_telemetry` runs at
/// process boot to install the tracing subscriber, long before the Postgres
/// pool exists, and reordering that would silently drop every boot log emitted
/// while the database connects. So the gate is created without a store and
/// [`SamplingGate::set_store`] fills it in once `main` has built the stores;
/// until then every lookup fails open and no span is dropped.
pub struct SamplingGate {
    store: RwLock<Option<Arc<dyn SamplingPolicyStore>>>,
    /// HMAC key for [`sampled_by_hash`], set the same way and for the same
    /// reason as `store` (see [`SamplingGate::set_sampling_secret`]). Empty
    /// until then, which only ever overlaps process boot before the server
    /// accepts requests.
    secret: RwLock<Vec<u8>>,
    snapshot: RwLock<Option<PolicySnapshot>>,
    /// One refresh in flight at a time. Concurrent stale reads all serve the
    /// previous snapshot rather than queueing duplicate queries.
    refreshing: AtomicBool,
    force_kept: TraceHistory,
    outcome_overrides: TraceHistory,
    /// Weak index only: request contexts and participating span processors own
    /// the token. Completed history never pins a live token. The index therefore
    /// scales with live frontend traces, not requests served in the last TTL.
    active: DashMap<[u8; 16], Weak<TraceDecision>>,
}

impl std::fmt::Debug for SamplingGate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SamplingGate")
            .field(
                "store_configured",
                &self.store.read().map(|s| s.is_some()).unwrap_or(false),
            )
            .field("force_kept_traces", &self.force_kept.values.len())
            .field("outcome_overrides", &self.outcome_overrides.values.len())
            .field("active_traces", &self.active.len())
            .finish_non_exhaustive()
    }
}

impl Default for SamplingGate {
    fn default() -> Self {
        Self::new()
    }
}

impl SamplingGate {
    pub fn new() -> Self {
        Self {
            store: RwLock::new(None),
            secret: RwLock::new(Vec::new()),
            snapshot: RwLock::new(None),
            refreshing: AtomicBool::new(false),
            force_kept: TraceHistory::new(MAX_FORCE_KEPT_TRACES),
            outcome_overrides: TraceHistory::new(MAX_OUTCOME_OVERRIDES),
            active: DashMap::new(),
        }
    }

    /// Point the gate at the environment's sampling policy. Called once, from
    /// `main`, after the stores are built.
    pub fn set_store(&self, store: Arc<dyn SamplingPolicyStore>) {
        match self.store.write() {
            Ok(mut slot) => *slot = Some(store),
            Err(poisoned) => {
                *poisoned.into_inner() = Some(store);
                self.store.clear_poison();
            }
        }
    }

    /// Give the gate the HMAC key [`sampled_by_hash`] uses, so the keep/drop
    /// mapping is unpredictable to a caller who controls the key it hashes
    /// (`session.id`, or a self-supplied `traceparent`'s trace id). Called
    /// once, from `main`, with the environment's own `ROUTER_KEY_PEPPER`
    /// (domain-separated — see `KeyHasher::sampling_secret`).
    pub fn set_sampling_secret(&self, secret: Vec<u8>) {
        match self.secret.write() {
            Ok(mut slot) => *slot = secret,
            Err(poisoned) => {
                *poisoned.into_inner() = secret;
                self.secret.clear_poison();
            }
        }
    }

    /// Read the policy now rather than waiting for an ending span to trigger a
    /// refresh, so the first requests after boot are decided by the policy
    /// instead of failing open for a TTL window. A failure here is logged and
    /// left to the next refresh; boot never depends on it.
    pub async fn refresh(&self) {
        let store = match self.store.read() {
            Ok(store) => store.clone(),
            Err(poisoned) => {
                let store = poisoned.into_inner().clone();
                self.store.clear_poison();
                store
            }
        };
        let Some(store) = store else {
            return;
        };
        match store.get().await {
            Ok(policy) => self.install(Some(policy)),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "Failed to read the sampling policy; keeping every frontend trace until it can be read"
                );
                self.install(None);
            }
        }
    }

    /// The policy to decide with, or `None` when none has ever been read
    /// (no store yet, or every read so far failed) — which callers treat as
    /// [`FAIL_OPEN_RATE`].
    fn policy(self: &Arc<Self>) -> Option<Arc<CachedSamplingPolicy>> {
        let (policy, fresh) = match self.snapshot.read() {
            Ok(snapshot) => match snapshot.as_ref() {
                Some(snapshot) => (
                    snapshot.policy.clone(),
                    snapshot.last_attempt.elapsed() < POLICY_SNAPSHOT_TTL,
                ),
                None => (None, false),
            },
            Err(poisoned) => {
                let policy = poisoned
                    .into_inner()
                    .as_ref()
                    .and_then(|s| s.policy.clone());
                self.snapshot.clear_poison();
                (policy, false)
            }
        };
        if !fresh {
            self.spawn_refresh();
        }
        policy
    }

    /// Refresh the snapshot off the request path.
    ///
    /// Both readers — [`Self::decide`] while a request is being admitted, and
    /// `on_end` on a tokio worker inside the request that produced the span —
    /// are synchronous, so the store's async read cannot happen here. The
    /// request being decided uses the previous snapshot; the refreshed one
    /// serves the next TTL window.
    fn spawn_refresh(self: &Arc<Self>) {
        if self.refreshing.swap(true, Ordering::SeqCst) {
            return;
        }
        let store = match self.store.read() {
            Ok(store) => store.clone(),
            Err(poisoned) => {
                let store = poisoned.into_inner().clone();
                self.store.clear_poison();
                store
            }
        };
        let Some(store) = store else {
            self.refreshing.store(false, Ordering::SeqCst);
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.refreshing.store(false, Ordering::SeqCst);
            return;
        };
        let gate = self.clone();
        handle.spawn(async move {
            let result = store.get().await;
            let refreshed = match result {
                Ok(policy) => Some(policy),
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "Failed to refresh sampling policy; keeping the last known policy"
                    );
                    None
                }
            };
            gate.install(refreshed);
            gate.refreshing.store(false, Ordering::SeqCst);
        });
    }

    /// Install a refresh result. `None` records only the attempt, leaving the
    /// previous policy in place.
    fn install(&self, refreshed: Option<SamplingPolicy>) {
        let refreshed = refreshed.map(|policy| {
            Arc::new(CachedSamplingPolicy {
                chat_outcomes: CompiledChatOutcomeRules::new(&policy.outcome_rules),
                policy,
            })
        });
        let mut slot = match self.snapshot.write() {
            Ok(slot) => slot,
            Err(poisoned) => {
                let slot = poisoned.into_inner();
                self.snapshot.clear_poison();
                slot
            }
        };
        let previous = slot.as_ref().and_then(|s| s.policy.clone());
        *slot = Some(PolicySnapshot {
            policy: refreshed.or(previous),
            last_attempt: Instant::now(),
        });
    }

    /// What the policy says about one request: whether to keep its trace, and
    /// whether to record its content.
    ///
    /// Synchronous and non-blocking, because this runs while a request is being
    /// admitted: [`Self::policy`] reads a cached snapshot and hands any refresh
    /// to a background task rather than awaiting one. A policy that cannot be
    /// consulted keeps trace metadata ([`FAIL_OPEN_RATE`]) but cannot authorize
    /// content capture. An outage must not turn an unknown privacy policy into
    /// permission to export request or response bodies.
    ///
    /// One rule match settles both answers, so they cannot come from two
    /// different rules. Neither is the last word on export:
    /// [`Self::force_keep_trace`] still overrides `keep` for a request that
    /// errored or that a handler flagged.
    pub fn decide(
        self: &Arc<Self>,
        key: &str,
        ctx: &MatchContext,
        conversation_keyed: bool,
    ) -> RequestSampling {
        let resolved = match self.policy() {
            Some(policy) => policy.resolve(ctx, conversation_keyed),
            None => ResolvedSampling {
                rate: FAIL_OPEN_RATE,
                log_content: false,
            },
        };
        let secret = match self.secret.read() {
            Ok(secret) => secret.clone(),
            Err(poisoned) => {
                let secret = poisoned.into_inner().clone();
                self.secret.clear_poison();
                secret
            }
        };
        RequestSampling {
            keep: sampled_by_hash(key, resolved.rate, &secret),
            log_content: resolved.log_content,
        }
    }

    /// Whether the active policy has any response-content rules at all — used
    /// on the request path to decide whether a streaming handler needs to
    /// accumulate response content it would otherwise skip (see
    /// `crate::sampling_policy::OutcomeRule`), independent of whether content
    /// logging is enabled.
    pub fn has_outcome_rules(self: &Arc<Self>) -> bool {
        self.policy()
            .is_some_and(|policy| policy.enabled && !policy.outcome_rules.is_empty())
    }

    /// The first matching outcome rule's raw (unfloored) rate for `response`,
    /// read from the gate's already-cached policy snapshot — no new store
    /// read. A handler calls this once it has the full response body, and
    /// stamps the result via
    /// [`crate::handlers::trace_attributes::LlmSpanWriter::set_sampling_outcome_rate`];
    /// [`Self::resolve_outcome`] is what turns it into a keep/drop decision.
    pub fn matched_outcome_rate(self: &Arc<Self>, response: &serde_json::Value) -> Option<f64> {
        self.policy()?.matched_outcome_rate(response)
    }

    /// Whether Chat outcome rules need the assembled message body. Typed
    /// metadata-only selectors do not require content accumulation.
    pub fn needs_chat_outcome_content(self: &Arc<Self>) -> bool {
        self.policy()
            .is_some_and(|policy| policy.enabled && policy.chat_outcomes.needs_content())
    }

    pub fn matched_chat_outcome_rate(
        self: &Arc<Self>,
        choices: &[crate::types::ChatChoice],
        completion: Option<&crate::types::ChatCompletionResponse>,
    ) -> Option<f64> {
        let policy = self.policy()?;
        if !policy.enabled {
            return None;
        }
        policy
            .chat_outcomes
            .matched_rate(&policy.outcome_rules, choices, completion)
    }

    /// Pin every remaining span of `trace_id` for export.
    ///
    /// Spans end child-first, so a force-keep discovered mid-request (a flagged
    /// moderation check stamped on an LLM span) reaches the enclosing request
    /// span through this set rather than through an attribute the parent never
    /// sees. It cannot resurrect a span that already ended and was dropped —
    /// which is why the request-scoped force header travels as baggage instead,
    /// reaching every span of the request at its start.
    pub fn force_keep_trace(&self, trace_id: TraceId) {
        if let Some(state) = self.active_state(trace_id) {
            state.force_keep.store(true, Ordering::Relaxed);
        }
        self.force_kept.insert_refresh(trace_id.to_bytes(), true);
    }

    fn is_force_kept(&self, trace_id: TraceId) -> bool {
        let remembered = self.force_kept.get(trace_id.to_bytes()) == Some(true);
        if let Some(state) = self.active_state(trace_id) {
            if remembered {
                state.force_keep.store(true, Ordering::Relaxed);
            }
            return state.force_keep.load(Ordering::Relaxed);
        }
        remembered
    }

    fn active_state(&self, trace_id: TraceId) -> Option<Arc<TraceDecision>> {
        self.active
            .get(&trace_id.to_bytes())
            .and_then(|state| state.upgrade())
    }

    fn trace_decision(self: &Arc<Self>, trace_id: TraceId) -> Arc<TraceDecision> {
        let mut entry = self.active.entry(trace_id.to_bytes()).or_default();
        if let Some(state) = entry.upgrade() {
            return state;
        }
        let state = Arc::new(TraceDecision {
            trace_id,
            gate: Arc::downgrade(self),
            outcome: OnceLock::new(),
            force_keep: AtomicBool::new(false),
        });
        *entry = Arc::downgrade(&state);
        state
    }

    /// Pin a frontend trace for the lifetime of its request context. The tracing
    /// bridge creates SDK spans only at close, so SDK on_start alone is too late
    /// to retain an enclosing HTTP span's decision while its LLM child ends.
    pub(crate) fn with_trace_decision(self: &Arc<Self>, cx: Context, trace_id: TraceId) -> Context {
        cx.with_value(self.trace_decision(trace_id))
    }

    /// Resolve a matched [`crate::sampling_policy::OutcomeRule`]'s `raw_rate`
    /// into a keep/drop decision for `trace_id`, using the same hash
    /// ([`sampled_by_hash`]) and key precedence (`session.id` if given, else
    /// the trace id) as [`Self::decide`] — so the outcome-rule mapping is
    /// exactly as unpredictable to a caller, and exactly as deterministic
    /// across retries, as the original decision's.
    ///
    /// Idempotent per trace: the first span to resolve an outcome for a
    /// trace wins, mirroring "one rule match settles it" elsewhere in this
    /// policy. A later span of the same trace resolving a *different* rate
    /// (nested LLM calls or a fallback retry disagreeing on the rule) is
    /// logged but does not change the cached answer.
    ///
    /// Active frontend contexts share a first-writer cell independently of the
    /// bounded completed-history cache. Evicting history cannot split a live
    /// trace. Calls without a participating context use bounded history only.
    pub fn resolve_outcome(
        self: &Arc<Self>,
        trace_id: TraceId,
        key: &str,
        raw_rate: f64,
        conversation_keyed: bool,
    ) -> bool {
        if let Some(cached) = self.cached_outcome(trace_id) {
            return cached;
        }
        let floored = match self.policy() {
            Some(policy) => policy.floored_rate(raw_rate, conversation_keyed),
            None => raw_rate,
        };
        let secret = match self.secret.read() {
            Ok(secret) => secret.clone(),
            Err(poisoned) => {
                let secret = poisoned.into_inner().clone();
                self.secret.clear_poison();
                secret
            }
        };
        let keep = sampled_by_hash(key, floored, &secret);
        let resolve = || {
            self.outcome_overrides
                .insert_first(trace_id.to_bytes(), keep)
        };
        let resolved = match self.active_state(trace_id) {
            Some(state) => *state.outcome.get_or_init(resolve),
            None => resolve(),
        };
        if resolved != keep {
            tracing::debug!(
                trace_id = %trace_id,
                cached = resolved,
                computed = keep,
                "an outcome rule resolved differently for a trace already decided; keeping the first answer"
            );
        }
        resolved
    }

    fn cached_outcome(&self, trace_id: TraceId) -> Option<bool> {
        if let Some(state) = self.active_state(trace_id) {
            if let Some(outcome) = state.outcome.get() {
                return Some(*outcome);
            }
            return self
                .outcome_overrides
                .get(trace_id.to_bytes())
                .map(|remembered| *state.outcome.get_or_init(|| remembered));
        }
        self.outcome_overrides.get(trace_id.to_bytes())
    }
}

/// One shared decision for all live requests/spans with the same trace id.
/// Allocated for frontends even before outcome rules exist, so enabling a rule
/// during a long stream still shares the newly resolved answer. No response or
/// policy snapshot is retained. Spans ending before an outcome is known are
/// deliberately not buffered.
#[derive(Debug)]
struct TraceDecision {
    trace_id: TraceId,
    gate: Weak<SamplingGate>,
    outcome: OnceLock<bool>,
    force_keep: AtomicBool,
}

impl Drop for TraceDecision {
    fn drop(&mut self) {
        if let Some(gate) = self.gate.upgrade() {
            // A replacement can be installed after our last strong reference
            // disappears but before this destructor acquires the shard lock.
            // Never remove that newer trace's token.
            gate.active
                .remove_if(&self.trace_id.to_bytes(), |_, state| {
                    std::ptr::eq(state.as_ptr(), self)
                });
        }
    }
}

/// Bounded FIFO history. Lookups use the sharded map; new resolutions serialize
/// only a constant amount of queue work. Each insertion removes at most 16
/// expired entries plus one capacity eviction, never a scan of the live map.
/// Eviction is safe because active contexts pin their own first-writer cell.
struct TraceHistory {
    values: DashMap<[u8; 16], (Instant, bool)>,
    order: Mutex<VecDeque<([u8; 16], Instant)>>,
    capacity: usize,
}

impl TraceHistory {
    const EXPIRY_BATCH: usize = 16;

    fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            values: DashMap::new(),
            order: Mutex::new(VecDeque::new()),
            capacity,
        }
    }

    fn get(&self, trace: [u8; 16]) -> Option<bool> {
        self.values
            .get(&trace)
            .filter(|entry| entry.0.elapsed() < FORCE_KEEP_TTL)
            .map(|entry| entry.1)
    }

    fn insert_first(&self, trace: [u8; 16], keep: bool) -> bool {
        self.insert_at(trace, keep, Instant::now()).0
    }

    fn insert_refresh(&self, trace: [u8; 16], keep: bool) {
        self.insert_at_inner(trace, keep, Instant::now(), true);
    }

    fn insert_at(&self, trace: [u8; 16], keep: bool, now: Instant) -> (bool, usize) {
        self.insert_at_inner(trace, keep, now, false)
    }

    fn insert_at_inner(
        &self,
        trace: [u8; 16],
        keep: bool,
        now: Instant,
        refresh: bool,
    ) -> (bool, usize) {
        let mut order = match self.order.lock() {
            Ok(order) => order,
            Err(poisoned) => {
                self.order.clear_poison();
                poisoned.into_inner()
            }
        };
        let mut expired = 0;
        while expired < Self::EXPIRY_BATCH
            && order
                .front()
                .is_some_and(|(_, marked)| now.saturating_duration_since(*marked) >= FORCE_KEEP_TTL)
        {
            let (key, marked) = order.pop_front().expect("front was present");
            self.values.remove_if(&key, |_, value| value.0 == marked);
            expired += 1;
        }
        if let Some(entry) = self.values.get(&trace) {
            if !refresh && now.saturating_duration_since(entry.0) < FORCE_KEEP_TTL {
                return (entry.1, expired);
            }
            // Expired or explicitly refreshed force-keep: leave its old queue
            // node to be reclaimed in order, avoiding a search of the queue.
        }
        // Queue nodes and map entries must both stay within the cap, including
        // when an expired trace is reused before its old queue node is visited.
        if order.len() >= self.capacity {
            let (key, marked) = order.pop_front().expect("full queue");
            self.values.remove_if(&key, |_, value| value.0 == marked);
        }
        self.values.insert(trace, (now, keep));
        order.push_back((trace, now));
        (keep, expired)
    }
}

/// Attach a request's sampling inputs, and the decision resolved from them, to
/// the context every span of that request inherits, and drop any the caller
/// tried to supply.
///
/// They ride in baggage for the same reason `session.id` does (see
/// [`crate::telemetry::SessionIdSpanProcessor`]): they are request-scoped facts
/// that *every* span of the request has to carry, and baggage is the only
/// channel a span created deep inside a handler still sees. Carrying them only
/// on the request span would decide the request span at the policy's rate and
/// its children at the default rate — splitting a trace precisely where the
/// content lives.
///
/// The reserved keys are stripped from the inbound baggage first. Baggage is
/// caller-supplied, so without that a client could force-keep every trace,
/// claim its trace was already decided keep, or turn content logging back on
/// for a key an operator had silenced, by putting these keys in its `baggage`
/// header. The caller's other baggage — `session.id` above all — is preserved.
pub(crate) fn with_sampling_inputs(
    cx: Context,
    request_tag: Option<String>,
    force_keep: bool,
    resolved: Option<RequestSampling>,
) -> Context {
    use opentelemetry::baggage::BaggageExt;

    const RESERVED: [&str; 4] = [
        CALLER_REQUEST_TAG_ATTRIBUTE,
        SAMPLING_FORCE_KEEP_ATTRIBUTE,
        SAMPLING_DECISION_ATTRIBUTE,
        SAMPLING_LOG_CONTENT_ATTRIBUTE,
    ];
    let spoofed = cx
        .baggage()
        .iter()
        .any(|(key, _)| RESERVED.contains(&key.as_str()));
    if !spoofed && request_tag.is_none() && !force_keep && resolved.is_none() {
        return cx;
    }

    let mut baggage: Vec<KeyValue> = cx
        .baggage()
        .iter()
        .filter(|(key, _)| !RESERVED.contains(&key.as_str()))
        .map(|(key, (value, _))| KeyValue::new(key.clone(), value.clone()))
        .collect();
    if let Some(tag) = request_tag {
        baggage.push(KeyValue::new(CALLER_REQUEST_TAG_ATTRIBUTE, tag));
    }
    if force_keep {
        baggage.push(KeyValue::new(SAMPLING_FORCE_KEEP_ATTRIBUTE, "true"));
    }
    if let Some(resolved) = resolved {
        // Spelled out rather than carried by presence alone: "keep", "drop" and
        // "this trace was never scoped by the policy" are three different
        // answers at the gate, and the third one forwards unconditionally.
        baggage.push(KeyValue::new(
            SAMPLING_DECISION_ATTRIBUTE,
            bool_baggage(resolved.keep),
        ));
        baggage.push(KeyValue::new(
            SAMPLING_LOG_CONTENT_ATTRIBUTE,
            bool_baggage(resolved.log_content),
        ));
    }
    cx.with_cleared_baggage().with_baggage(baggage)
}

fn bool_baggage(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

/// Read one of this module's boolean baggage values. Absent, or anything other
/// than the two spellings it writes, is `None` — a value it did not write must
/// never be read as a decision.
pub(crate) fn bool_from_baggage(cx: &Context, key: &str) -> Option<bool> {
    use opentelemetry::baggage::BaggageExt;

    cx.baggage()
        .get(key)
        .and_then(|value| match value.as_str().as_ref() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        })
}

/// Copies the request's keep decision, and the inputs it was resolved from,
/// from baggage onto every span it starts — so the gate reads one trace-wide
/// answer off whichever span it is handed.
///
/// A span processor rather than a stamp at each span site, for the reason
/// [`crate::telemetry::SessionIdSpanProcessor`] gives: it sees every span the
/// process starts, so a new span cannot silently leave its request's decision
/// behind and be exported outside the policy's rate instead.
#[derive(Debug, Default)]
pub struct SamplingContextSpanProcessor;

impl SpanProcessor for SamplingContextSpanProcessor {
    fn on_start(&self, span: &mut Span, cx: &Context) {
        use opentelemetry::baggage::BaggageExt;
        use opentelemetry::trace::Span as _;

        // `cx` is the span's parent context, not necessarily the current one.
        if let Some(tag) = cx.baggage().get(CALLER_REQUEST_TAG_ATTRIBUTE) {
            span.set_attribute(KeyValue::new(CALLER_REQUEST_TAG_ATTRIBUTE, tag.clone()));
        }
        if cx
            .baggage()
            .get(SAMPLING_FORCE_KEEP_ATTRIBUTE)
            .is_some_and(|value| value.as_str() == "true")
        {
            span.set_attribute(KeyValue::new(SAMPLING_FORCE_KEEP_ATTRIBUTE, true));
        }
        // Absent stays absent: a trace the policy never scoped must not acquire
        // a decision here, because the gate forwards those unconditionally.
        if let Some(decision) = bool_from_baggage(cx, SAMPLING_DECISION_ATTRIBUTE) {
            span.set_attribute(KeyValue::new(SAMPLING_DECISION_ATTRIBUTE, decision));
        }
        // `log_content` is deliberately not stamped: handler code reads it from
        // baggage as it writes a span, and the export path has no use for it.
    }

    fn on_end(&self, _span: SpanData) {}

    fn force_flush(&self) -> TraceResult<()> {
        Ok(())
    }

    fn shutdown(&self) -> TraceResult<()> {
        Ok(())
    }
}

/// The gate's inputs, read in one pass over a span's attributes: whether the
/// span is pinned by a force-keep, its trace's rate-based decision, a
/// response-derived outcome rate this span resolved (if any), and the
/// session id this span carries (if any) — the same key
/// [`SamplingGate::resolve_outcome`] must use to agree with the original
/// decision's.
///
/// `decision: None` means the span's trace was never scoped by the policy —
/// a route that is not an LLM frontend — and is forwarded untouched. A span
/// may carry over a hundred attributes, so this scans the list once rather
/// than once per key.
#[derive(Debug, PartialEq)]
struct SamplingVerdictInputs {
    force_keep: bool,
    decision: Option<bool>,
    outcome_rate: Option<f64>,
    session_id: Option<String>,
}

fn sampling_verdict(span: &SpanData) -> SamplingVerdictInputs {
    let mut inputs = SamplingVerdictInputs {
        force_keep: false,
        decision: None,
        outcome_rate: None,
        session_id: None,
    };
    for kv in &span.attributes {
        match (kv.key.as_str(), &kv.value) {
            (SAMPLING_FORCE_KEEP_ATTRIBUTE, opentelemetry::Value::Bool(value)) => {
                inputs.force_keep = *value;
            }
            (SAMPLING_DECISION_ATTRIBUTE, opentelemetry::Value::Bool(value)) => {
                inputs.decision = Some(*value);
            }
            (SAMPLING_OUTCOME_RATE_ATTRIBUTE, opentelemetry::Value::F64(value)) => {
                inputs.outcome_rate = Some(*value);
            }
            (SESSION_ID_ATTRIBUTE, opentelemetry::Value::String(value)) => {
                inputs.session_id = Some(value.as_str().to_string());
            }
            _ => {}
        }
    }
    inputs
}

/// Authenticated environment context propagated through spans and background work.
#[derive(Clone, Debug)]
pub struct TenantSampling {
    pub environment_id: String,
    pub gate: Arc<SamplingGate>,
}

/// Installed once per provider, before the destination-specific gates. The SDK
/// appends attributes, so stamping in each gate would consume duplicate slots.
#[derive(Debug, Default)]
pub struct TenantAttributionSpanProcessor;

impl SpanProcessor for TenantAttributionSpanProcessor {
    fn on_start(&self, span: &mut Span, cx: &Context) {
        use opentelemetry::trace::Span as _;
        if let Some(tenant) = cx.get::<TenantSampling>() {
            span.set_attribute(KeyValue::new(
                super::ENVIRONMENT_ID_ATTRIBUTE,
                tenant.environment_id.clone(),
            ));
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

#[derive(Debug)]
struct SpanGate {
    gate: Arc<SamplingGate>,
    _decision: Option<Arc<TraceDecision>>,
}

fn environment_attribute(span: &SpanData) -> String {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == super::ENVIRONMENT_ID_ATTRIBUTE)
        .map(|kv| kv.value.as_str().to_string())
        .unwrap_or_default()
}

/// Gates the OTLP export of LLM-frontend spans on the environment's sampling
/// policy, forwarding everything else unchanged.
///
/// The rate-based decision is already made and already on the span
/// ([`crate::telemetry::SAMPLING_DECISION_ATTRIBUTE`]), so this reads it
/// uniformly rather than asking what kind of span it is holding. What is left
/// to do here is the half of the decision that only the finished request knows:
/// a handler that flags an outcome pins its trace through
/// [`SamplingGate::force_keep_trace`], which overrides the rate.
#[derive(Debug)]
pub struct SamplingGateSpanProcessor<P: SpanProcessor> {
    inner: P,
    gate: Arc<SamplingGate>,
    active_spans: DashMap<(String, TraceId, SpanId), SpanGate>,
}

impl<P: SpanProcessor> SamplingGateSpanProcessor<P> {
    pub fn new(inner: P, gate: Arc<SamplingGate>) -> Self {
        Self {
            inner,
            gate,
            active_spans: DashMap::new(),
        }
    }

    /// Hand a kept span to the exporter, without the gate's own bookkeeping.
    ///
    /// Only kept spans get here, so the decision attribute is `true` on every
    /// span that would carry it out — no information, one slot of the per-span
    /// attribute budget [`crate::handlers::trace_attributes::LlmSpanWriter`]
    /// allocates against, and one more attribute on the wire per span. The
    /// rate, the tag and the force-keep flag are left on: those still say
    /// something about a trace an operator is reading.
    fn forward(&self, mut span: SpanData) {
        span.attributes
            .retain(|kv| kv.key.as_str() != SAMPLING_DECISION_ATTRIBUTE);
        self.inner.on_end(span);
    }
}

impl<P: SpanProcessor> SpanProcessor for SamplingGateSpanProcessor<P> {
    fn on_start(&self, span: &mut Span, cx: &Context) {
        // Every span still reaches the inner processor's `on_start`: the batch
        // processor does no work there, and filtering at start would decide
        // before the attributes this gate reads exist.
        use opentelemetry::trace::Span as _;

        let tenant = cx.get::<TenantSampling>();
        let gate = tenant.map(|tenant| &tenant.gate).unwrap_or(&self.gate);
        let sc = span.span_context();
        let decision = cx.get::<Arc<TraceDecision>>().cloned().or_else(|| {
            bool_from_baggage(cx, SAMPLING_DECISION_ATTRIBUTE)
                .map(|_| gate.trace_decision(sc.trace_id()))
        });
        // Ordinary single-environment spans have no decision or tenant-specific
        // gate to retain. Keep them off the active-span map and avoid clones.
        if decision.is_some() || !Arc::ptr_eq(gate, &self.gate) {
            let environment = tenant
                .map(|tenant| tenant.environment_id.clone())
                .unwrap_or_default();
            self.active_spans.insert(
                (environment, sc.trace_id(), sc.span_id()),
                SpanGate {
                    gate: gate.clone(),
                    _decision: decision,
                },
            );
        }
        self.inner.on_start(span, cx);
    }

    fn on_end(&self, span: SpanData) {
        // Retain through forwarding/dropping, and through both destinations'
        // callbacks. A cancelled request releases its Context automatically;
        // SDK Span Drop still balances this on_end registration.
        let _active = self.active_spans.remove(&(
            environment_attribute(&span),
            span.span_context.trace_id(),
            span.span_context.span_id(),
        ));
        let gate = _active
            .as_ref()
            .map(|(_, state)| &state.gate)
            .unwrap_or(&self.gate);
        let verdict = sampling_verdict(&span);
        let trace_id = span.span_context.trace_id();

        if verdict.force_keep {
            gate.force_keep_trace(trace_id);
            self.forward(span);
            return;
        }
        // Spans end child-first, so a force-keep a handler discovered mid-request
        // reaches the enclosing request span through the gate's trace-id set
        // rather than through an attribute the parent never sees. Either form
        // overrides the rate.
        if gate.is_force_kept(trace_id) {
            self.forward(span);
            return;
        }
        // An outcome rule fires from the response, so it can only ever be
        // resolved by the one span that saw it — every other span of the
        // trace (including one ending first, or one that opened before the
        // response existed) has to learn the resolved keep/drop from the
        // gate rather than from its own attributes. Unlike force-keep, this
        // can override the rate-based decision in *either* direction, so it
        // sits ahead of `decision` in precedence but behind an unconditional
        // force-keep, which must still win for an errored request even if an
        // outcome rule also matched and said "drop".
        let outcome = match verdict.outcome_rate {
            Some(rate) => {
                let fallback_key = trace_id.to_string();
                let key = verdict.session_id.as_deref().unwrap_or(&fallback_key);
                Some(gate.resolve_outcome(trace_id, key, rate, verdict.session_id.is_some()))
            }
            None => gate.cached_outcome(trace_id),
        };
        // No decision at all means this span's trace was never scoped as a
        // frontend route, which forwards unconditionally — that is what leaves
        // admin RPCs, health probes and gRPC governed by `OTEL_TRACES_SAMPLER`
        // alone.
        if outcome.unwrap_or_else(|| verdict.decision.unwrap_or(true)) {
            self.forward(span);
        }
    }

    fn force_flush(&self) -> TraceResult<()> {
        self.inner.force_flush()
    }

    fn shutdown(&self) -> TraceResult<()> {
        self.active_spans.clear();
        self.inner.shutdown()
    }

    fn set_resource(&mut self, resource: &Resource) {
        // Forwarded so a wrapped OTLP batch processor still learns the
        // provider's resource (service name, environment id), which its
        // exporter has no other channel for. Harmless for the Braintrust
        // processor, whose own `set_resource` is a no-op because its exporter
        // was given the resource at construction.
        self.inner.set_resource(resource);
    }
}

/// Records every LLM-frontend span, and leaves every other route to the
/// `OTEL_TRACES_SAMPLER`-configured sampler.
///
/// The SDK allows one sampler per provider, so the frontend/everything-else
/// split has to happen inside a sampler rather than by installing two. A
/// sampler never sees the HTTP request, only what is already on the span
/// builder, so the route is recovered from the `http.target` attribute
/// [`crate::telemetry::OtelMakeSpan`] sets at creation — the `chalk.frontend`
/// attributes are added after the builder exists and are not reliably visible
/// here.
///
/// A span whose parent is already sampled is recorded too. Descendants of a
/// recorded frontend span (the handlers' LLM spans, and anything else opened
/// while the request is served) carry no `http.target` of their own, and a ratio
/// sampler applied to them independently would cut the body out of a trace whose
/// root was kept. Recording them costs no export volume: every one of them
/// carries its trace's keep decision, so [`SamplingGateSpanProcessor`] drops
/// them with the trace.
/// A deployment whose `OTEL_TRACES_SAMPLER` switches tracing off is the one
/// exception: `record_frontends` is false there, so the off switch is not
/// quietly overridden by a policy the operator may never have looked at.
#[derive(Debug, Clone)]
pub struct FrontendRouteSampler {
    /// The `OTEL_TRACES_SAMPLER` sampler, unchanged, for every non-frontend span.
    default_sampler: Sampler,
    record_frontends: bool,
}

impl FrontendRouteSampler {
    pub fn new(default_sampler: Sampler, record_frontends: bool) -> Self {
        Self {
            default_sampler,
            record_frontends,
        }
    }
}

/// Whether the span being created belongs to an LLM frontend route.
fn is_frontend_builder(name: &str, attributes: &[KeyValue]) -> bool {
    let target = attributes.iter().find_map(|kv| {
        (kv.key.as_str() == "http.target").then(|| match &kv.value {
            opentelemetry::Value::String(value) => Some(value.as_str().to_string()),
            _ => None,
        })?
    });
    // `otel.name` is `"<METHOD> <path>"`, so the name is the fallback when a
    // future span shape omits `http.target`.
    let candidate = target.or_else(|| {
        name.split_once(' ')
            .map(|(_, path)| path.to_string())
            .filter(|path| path.starts_with('/'))
    });
    candidate
        .map(|candidate| {
            let path = candidate
                .split('?')
                .next()
                .unwrap_or(candidate.as_str())
                .to_string();
            frontend_for_path(&path).is_some()
        })
        .unwrap_or(false)
}

fn parent_is_sampled(parent_context: Option<&Context>) -> bool {
    use opentelemetry::trace::TraceContextExt;
    parent_context
        .filter(|cx| cx.has_active_span())
        .map(|cx| cx.span().span_context().is_sampled())
        .unwrap_or(false)
}

impl ShouldSample for FrontendRouteSampler {
    fn should_sample(
        &self,
        parent_context: Option<&Context>,
        trace_id: TraceId,
        name: &str,
        span_kind: &opentelemetry::trace::SpanKind,
        attributes: &[KeyValue],
        links: &[opentelemetry::trace::Link],
    ) -> SamplingResult {
        let record = self.record_frontends
            && (is_frontend_builder(name, attributes) || parent_is_sampled(parent_context));
        let sampler = if record {
            &Sampler::AlwaysOn
        } else {
            &self.default_sampler
        };
        sampler.should_sample(parent_context, trace_id, name, span_kind, attributes, links)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::SESSION_ID_ATTRIBUTE;

    #[test]
    fn unscoped_default_spans_do_not_allocate_active_entries() {
        use opentelemetry::baggage::BaggageExt;
        use opentelemetry::trace::{Span as _, Tracer as _, TracerProvider as _};
        let gate = Arc::new(SamplingGate::new());
        let processor = SamplingGateSpanProcessor::new(SamplingContextSpanProcessor, gate.clone());
        let provider = opentelemetry_sdk::trace::TracerProvider::builder().build();
        let tracer = provider.tracer("gate-bookkeeping");
        for cx in [
            Context::new(),
            Context::new().with_value(TenantSampling {
                environment_id: "default".into(),
                gate: gate.clone(),
            }),
        ] {
            let mut span = tracer.start("ordinary");
            processor.on_start(&mut span, &cx);
            assert!(processor.active_spans.is_empty());
            span.end();
        }
        let tenant = Context::new().with_value(TenantSampling {
            environment_id: "other".into(),
            gate: Arc::new(SamplingGate::new()),
        });
        let mut span = tracer.start("tenant");
        TenantAttributionSpanProcessor.on_start(&mut span, &tenant);
        processor.on_start(&mut span, &tenant);
        assert_eq!(processor.active_spans.len(), 1);
        processor.on_end(span.exported_data().unwrap());
        assert!(processor.active_spans.is_empty());
        span.end();
        let cx = Context::new().with_baggage([KeyValue::new(SAMPLING_DECISION_ATTRIBUTE, "false")]);
        let mut span = tracer.start("sampled");
        processor.on_start(&mut span, &cx);
        assert_eq!(processor.active_spans.len(), 1);
        processor.on_end(span.exported_data().unwrap());
        assert!(processor.active_spans.is_empty());
        span.end();
        provider.shutdown().unwrap();
    }

    const TEST_SECRET: &[u8] = b"unit-test-sampling-secret";

    #[test]
    fn hashing_the_same_key_always_gives_the_same_answer() {
        for rate in [0.01, 0.25, 0.5, 0.99] {
            let first = sampled_by_hash("session-abcdef", rate, TEST_SECRET);
            for _ in 0..100 {
                assert_eq!(
                    sampled_by_hash("session-abcdef", rate, TEST_SECRET),
                    first,
                    "the decision for one key must not vary at rate {rate}"
                );
            }
        }
    }

    #[test]
    fn the_secret_changes_the_decision() {
        // A different environment's secret must not agree with this one's,
        // or a caller who learned one environment's mapping could carry it
        // over to another.
        let rate = 0.5;
        let by_env_a: Vec<bool> = (0..200)
            .map(|i| sampled_by_hash(&format!("session-{i}"), rate, TEST_SECRET))
            .collect();
        let by_env_b: Vec<bool> = (0..200)
            .map(|i| sampled_by_hash(&format!("session-{i}"), rate, b"a different secret"))
            .collect();
        assert_ne!(
            by_env_a, by_env_b,
            "two different secrets produced the same 200 decisions"
        );
    }

    #[test]
    fn the_extremes_are_absolute() {
        for key in ["a", "b", "0af7651916cd43dd8448eb211c80319c"] {
            assert!(sampled_by_hash(key, 1.0, TEST_SECRET));
            assert!(
                sampled_by_hash(key, 2.0, TEST_SECRET),
                "rates above 1 keep everything"
            );
            assert!(!sampled_by_hash(key, 0.0, TEST_SECRET));
            assert!(
                !sampled_by_hash(key, -1.0, TEST_SECRET),
                "rates below 0 drop everything"
            );
            assert!(
                sampled_by_hash(key, f64::NAN, TEST_SECRET),
                "an unreadable rate must keep the span, not drop it"
            );
        }
    }

    #[test]
    fn distinct_keys_are_kept_at_roughly_the_configured_rate() {
        // 20k keys is enough that a correct implementation lands well inside
        // the tolerance while a broken one (constant hash, wrong bit shift,
        // comparison inverted) lands far outside it. The keys and the hash are
        // both fixed, so this is deterministic rather than flaky.
        const SAMPLES: usize = 20_000;
        for rate in [0.05, 0.2, 0.5, 0.8] {
            let kept = (0..SAMPLES)
                .filter(|i| sampled_by_hash(&format!("session-{i}"), rate, TEST_SECRET))
                .count();
            let observed = kept as f64 / SAMPLES as f64;
            assert!(
                (observed - rate).abs() < 0.02,
                "rate {rate} kept {observed} of distinct keys"
            );
        }
    }

    #[test]
    fn the_unit_interval_stays_in_range() {
        for i in 0..1_000 {
            let value = unit_interval(&format!("key-{i}"), TEST_SECRET);
            assert!((0.0..1.0).contains(&value), "{value} out of range");
        }
    }

    #[test]
    fn session_and_trace_keys_decide_independently() {
        // Two requests of one conversation share a session id but not a trace
        // id: keying on the session is what makes their decisions agree.
        let rate = 0.5;
        let session = "conversation-7";
        assert_eq!(
            sampled_by_hash(session, rate, TEST_SECRET),
            sampled_by_hash(session, rate, TEST_SECRET)
        );
        let by_trace_a = sampled_by_hash("0af7651916cd43dd8448eb211c80319c", rate, TEST_SECRET);
        let by_trace_b = sampled_by_hash("1bf7651916cd43dd8448eb211c80319d", rate, TEST_SECRET);
        assert_ne!(
            by_trace_a, by_trace_b,
            "distinct trace ids must be able to disagree at rate 0.5"
        );
    }

    #[test]
    fn frontend_routes_are_recognized_from_the_builder() {
        let target = |value: &str| vec![KeyValue::new("http.target", value.to_string())];

        assert!(is_frontend_builder(
            "POST /v1/chat/completions",
            &target("/v1/chat/completions")
        ));
        assert!(is_frontend_builder(
            "POST /v1/messages",
            &target("/v1/messages?beta=1")
        ));
        // Name-only fallback when no `http.target` is present.
        assert!(is_frontend_builder("POST /v1/embeddings", &[]));
        // Admin, ingest and gRPC routes keep the environment's sampler.
        assert!(!is_frontend_builder(
            "GET /v1/admin/keys",
            &target("/v1/admin/keys")
        ));
        assert!(!is_frontend_builder(
            "POST /v1/traces/spans",
            &target("/v1/traces/spans")
        ));
        assert!(!is_frontend_builder("grpc_request", &[]));
    }

    #[test]
    fn frontend_spans_are_recorded_over_a_ratio_sampler_but_not_over_the_off_switch() {
        use opentelemetry::trace::{SamplingDecision, SpanKind};

        let target = vec![KeyValue::new("http.target", "/v1/chat/completions")];
        let trace_id = TraceId::from_hex("0af7651916cd43dd8448eb211c80319c").unwrap();
        let decide = |sampler: &FrontendRouteSampler, attributes: &[KeyValue]| {
            sampler
                .should_sample(
                    None,
                    trace_id,
                    "POST /v1/chat/completions",
                    &SpanKind::Server,
                    attributes,
                    &[],
                )
                .decision
        };

        // A ratio sampler would drop nearly everything; frontend spans are
        // recorded anyway and thinned at export instead.
        let thinning = FrontendRouteSampler::new(Sampler::TraceIdRatioBased(0.0), true);
        assert_eq!(
            decide(&thinning, &target),
            SamplingDecision::RecordAndSample
        );
        assert_eq!(
            decide(&thinning, &[KeyValue::new("http.target", "/v1/admin/keys")]),
            SamplingDecision::Drop,
            "non-frontend routes keep the environment's sampler"
        );

        // `always_off` means "do not trace", and stays meaning that.
        let disabled = FrontendRouteSampler::new(Sampler::AlwaysOff, false);
        assert_eq!(decide(&disabled, &target), SamplingDecision::Drop);
    }

    /// A match context naming only a request tag.
    fn tagged(tag: &str) -> MatchContext {
        MatchContext {
            request_tag: Some(tag.to_string()),
            ..MatchContext::default()
        }
    }

    #[tokio::test]
    async fn a_gate_without_a_store_keeps_every_span() {
        let gate = Arc::new(SamplingGate::new());
        assert!(gate.policy().is_none(), "no store yet -> no policy");
        assert!(sampled_by_hash("anything", FAIL_OPEN_RATE, TEST_SECRET));
        let resolved = gate.decide("ses-1", &MatchContext::default(), true);
        assert!(
            resolved.keep,
            "an unreadable policy must decide keep, never drop"
        );
        assert!(
            !resolved.log_content,
            "an unreadable policy cannot authorize content capture"
        );
    }

    #[tokio::test]
    async fn decide_resolves_the_matching_rule_for_a_request() {
        use crate::sampling_policy::{InMemorySamplingPolicyStore, SamplingRule};

        let policy = SamplingPolicy {
            enabled: true,
            default_rate: 0.0,
            rules: vec![SamplingRule {
                request_tag: Some("kept".to_string()),
                key_id: None,
                usage_pool_id: None,
                rate: 1.0,
                log_content: false,
            }],
            ..SamplingPolicy::default()
        };
        let gate = Arc::new(SamplingGate::new());
        gate.set_store(Arc::new(InMemorySamplingPolicyStore::new(policy)));
        gate.refresh().await;

        let unmatched = gate.decide("ses-1", &MatchContext::default(), false);
        assert!(!unmatched.keep, "no rule matched -> default_rate 0");
        assert!(!unmatched.log_content, "-> default_log_content");

        let matched = gate.decide("ses-1", &tagged("kept"), false);
        assert!(matched.keep, "the rule's rate keeps the trace");
        assert!(
            !matched.log_content,
            "and the same rule settles content logging"
        );

        // A disabled policy keeps everything, which is what makes the off state
        // indistinguishable from not having this feature.
        let off = Arc::new(SamplingGate::new());
        off.set_store(Arc::new(InMemorySamplingPolicyStore::new(
            SamplingPolicy::default(),
        )));
        off.refresh().await;
        assert!(off.decide("ses-1", &tagged("kept"), false).keep);
    }

    #[tokio::test]
    async fn the_gate_serves_the_stored_policy_after_a_refresh() {
        use crate::sampling_policy::InMemorySamplingPolicyStore;

        let policy = SamplingPolicy {
            enabled: true,
            default_rate: 0.25,
            ..SamplingPolicy::default()
        };
        let gate = Arc::new(SamplingGate::new());
        gate.set_store(Arc::new(InMemorySamplingPolicyStore::new(policy)));

        // The first read triggers the refresh and returns nothing yet; failing
        // open is the documented behavior for that window.
        assert!(gate.policy().is_none());
        tokio::time::sleep(Duration::from_millis(50)).await;
        let refreshed = gate.policy().expect("the refresh installed a policy");
        assert!(refreshed.enabled);
        assert_eq!(
            refreshed.effective_rate(&MatchContext::default(), false),
            0.25
        );
    }

    #[test]
    fn policy_reads_share_rules_and_refresh_replaces_only_the_snapshot() {
        let gate = Arc::new(SamplingGate::new());
        gate.install(Some(SamplingPolicy::default()));
        let first = gate.policy().unwrap();
        let second = gate.policy().unwrap();
        assert!(Arc::ptr_eq(&first, &second));

        gate.install(Some(SamplingPolicy {
            default_rate: 0.25,
            ..SamplingPolicy::default()
        }));
        let refreshed = gate.policy().unwrap();
        assert!(!Arc::ptr_eq(&first, &refreshed));
        assert_eq!(first.default_rate, 1.0);
        assert_eq!(refreshed.default_rate, 0.25);
        gate.install(None);
        assert!(Arc::ptr_eq(&refreshed, &gate.policy().unwrap()));
    }

    #[test]
    fn chat_outcome_content_requirement_refreshes_with_the_cached_policy() {
        use crate::sampling_policy::{OutcomeComparator, OutcomeRule};
        let gate = Arc::new(SamplingGate::new());
        let mut policy = SamplingPolicy {
            enabled: true,
            outcome_rules: vec![OutcomeRule {
                path: "choices.0.finish_reason".into(),
                op: OutcomeComparator::Eq,
                value: serde_json::json!("stop"),
                rate: 1.0,
            }],
            ..SamplingPolicy::default()
        };
        gate.install(Some(policy.clone()));
        assert!(gate.has_outcome_rules());
        assert!(!gate.needs_chat_outcome_content());
        policy.outcome_rules[0].path = "choices.0.message.content".into();
        gate.install(Some(policy.clone()));
        assert!(gate.needs_chat_outcome_content());
        policy.enabled = false;
        gate.install(Some(policy));
        assert!(!gate.needs_chat_outcome_content());
        assert_eq!(gate.matched_chat_outcome_rate(&[], None), None);
    }

    #[test]
    fn caller_supplied_sampling_baggage_is_replaced_not_trusted() {
        use opentelemetry::baggage::BaggageExt;

        // A client sending the reserved keys in its `baggage` header could
        // otherwise force-keep every trace, claim its trace had already been
        // decided keep, or turn content logging back on for a silenced key.
        let spoofed = Context::new().with_baggage(vec![
            KeyValue::new(SESSION_ID_ATTRIBUTE, "ses-42"),
            KeyValue::new(SAMPLING_FORCE_KEEP_ATTRIBUTE, "true"),
            KeyValue::new(CALLER_REQUEST_TAG_ATTRIBUTE, "pretend_tag"),
            KeyValue::new(SAMPLING_DECISION_ATTRIBUTE, "true"),
            KeyValue::new(SAMPLING_LOG_CONTENT_ATTRIBUTE, "true"),
        ]);

        let cleaned = with_sampling_inputs(spoofed.clone(), None, false, None);
        let baggage = cleaned.baggage();
        assert_eq!(
            baggage
                .get(SESSION_ID_ATTRIBUTE)
                .map(|value| value.as_str().to_string()),
            Some("ses-42".to_string()),
            "the caller's own baggage must survive"
        );
        for key in [
            SAMPLING_FORCE_KEEP_ATTRIBUTE,
            CALLER_REQUEST_TAG_ATTRIBUTE,
            SAMPLING_DECISION_ATTRIBUTE,
            SAMPLING_LOG_CONTENT_ATTRIBUTE,
        ] {
            assert!(baggage.get(key).is_none(), "{key} must not be trusted");
        }

        // The router's own values replace them.
        let resolved = with_sampling_inputs(
            spoofed,
            Some("openai_complete".to_string()),
            true,
            Some(RequestSampling {
                keep: false,
                log_content: false,
            }),
        );
        let baggage = resolved.baggage();
        assert_eq!(
            baggage
                .get(CALLER_REQUEST_TAG_ATTRIBUTE)
                .map(|value| value.as_str().to_string()),
            Some("openai_complete".to_string())
        );
        assert_eq!(
            baggage
                .get(SAMPLING_FORCE_KEEP_ATTRIBUTE)
                .map(|value| value.as_str().to_string()),
            Some("true".to_string())
        );
        assert_eq!(
            bool_from_baggage(&resolved, SAMPLING_DECISION_ATTRIBUTE),
            Some(false),
            "a drop decision has to be spelled out, not left absent"
        );
        assert_eq!(
            bool_from_baggage(&resolved, SAMPLING_LOG_CONTENT_ATTRIBUTE),
            Some(false)
        );
    }

    #[test]
    fn the_decision_is_omitted_when_no_route_was_scoped() {
        use opentelemetry::baggage::BaggageExt;

        // A non-frontend route resolves nothing, and the gate reads the absent
        // decision as "forward unconditionally" while content logging stays as
        // the environment has it.
        let cx = with_sampling_inputs(Context::new(), None, false, None);
        assert!(cx.baggage().get(SAMPLING_DECISION_ATTRIBUTE).is_none());
        assert!(cx.baggage().get(SAMPLING_LOG_CONTENT_ATTRIBUTE).is_none());
        assert_eq!(bool_from_baggage(&cx, SAMPLING_LOG_CONTENT_ATTRIBUTE), None);
    }

    #[test]
    fn only_the_two_spellings_it_writes_are_read_back() {
        use opentelemetry::baggage::BaggageExt;

        // A value this module did not write must not become a decision.
        for value in ["TRUE", "1", "yes", ""] {
            let cx = Context::new()
                .with_baggage(vec![KeyValue::new(SAMPLING_DECISION_ATTRIBUTE, value)]);
            assert_eq!(
                bool_from_baggage(&cx, SAMPLING_DECISION_ATTRIBUTE),
                None,
                "{value:?} is not a decision"
            );
        }
    }

    #[test]
    fn the_verdict_reads_force_keep_and_the_decision_off_one_span() {
        use opentelemetry::trace::{SpanContext, SpanKind, Status};
        use opentelemetry_sdk::trace::SpanEvents;
        use opentelemetry_sdk::trace::SpanLinks;

        let span = |attributes: Vec<KeyValue>| SpanData {
            span_context: SpanContext::empty_context(),
            parent_span_id: opentelemetry::trace::SpanId::INVALID,
            span_kind: SpanKind::Internal,
            name: "db_budget_check".into(),
            start_time: std::time::SystemTime::UNIX_EPOCH,
            end_time: std::time::SystemTime::UNIX_EPOCH,
            attributes,
            dropped_attributes_count: 0,
            events: SpanEvents::default(),
            links: SpanLinks::default(),
            status: Status::Unset,
            instrumentation_scope: Default::default(),
        };

        // An untagged child of a frontend request: no LLM markers, but it
        // carries its trace's decision and is answered by it.
        assert_eq!(
            sampling_verdict(&span(vec![KeyValue::new(
                SAMPLING_DECISION_ATTRIBUTE,
                false
            )])),
            SamplingVerdictInputs {
                force_keep: false,
                decision: Some(false),
                outcome_rate: None,
                session_id: None,
            }
        );
        assert_eq!(
            sampling_verdict(&span(vec![
                KeyValue::new(SAMPLING_DECISION_ATTRIBUTE, false),
                KeyValue::new(SAMPLING_FORCE_KEEP_ATTRIBUTE, true),
            ])),
            SamplingVerdictInputs {
                force_keep: true,
                decision: Some(false),
                outcome_rate: None,
                session_id: None,
            },
            "a force-keep is reported alongside the rate decision it overrides"
        );
        assert_eq!(
            sampling_verdict(&span(vec![KeyValue::new("llm.system", "openai")])),
            SamplingVerdictInputs {
                force_keep: false,
                decision: None,
                outcome_rate: None,
                session_id: None,
            },
            "a span whose trace was never scoped carries no decision"
        );
        assert_eq!(
            sampling_verdict(&span(vec![
                KeyValue::new(SAMPLING_OUTCOME_RATE_ATTRIBUTE, 0.1),
                KeyValue::new(SESSION_ID_ATTRIBUTE, "ses-1"),
            ])),
            SamplingVerdictInputs {
                force_keep: false,
                decision: None,
                outcome_rate: Some(0.1),
                session_id: Some("ses-1".to_string()),
            },
            "an outcome rate and a session id are both read off the same span"
        );
    }

    #[test]
    fn force_kept_traces_are_remembered_and_bounded() {
        let gate = SamplingGate::new();
        let trace = TraceId::from_hex("0af7651916cd43dd8448eb211c80319c").unwrap();
        assert!(!gate.is_force_kept(trace));
        gate.force_keep_trace(trace);
        assert!(gate.is_force_kept(trace));

        for i in 0..MAX_FORCE_KEPT_TRACES as u128 + 100 {
            gate.force_keep_trace(TraceId::from(i + 1));
        }
        assert!(
            gate.force_kept.values.len() <= MAX_FORCE_KEPT_TRACES,
            "the force-keep set must stay bounded"
        );
    }

    #[test]
    fn resolve_outcome_matches_the_hash_a_matching_decide_call_would() {
        let gate = Arc::new(SamplingGate::new());
        let trace = TraceId::from_hex("0af7651916cd43dd8448eb211c80319c").unwrap();
        let rate = 0.5;
        let key = "ses-outcome-1";
        // Same key, same rate, same (empty, unset) secret as `decide` would use.
        let expected = sampled_by_hash(key, rate, &[]);
        assert_eq!(gate.resolve_outcome(trace, key, rate, true), expected);
    }

    #[test]
    fn resolve_outcome_is_idempotent_per_trace() {
        let gate = Arc::new(SamplingGate::new());
        let trace = TraceId::from_hex("0af7651916cd43dd8448eb211c80319c").unwrap();
        // Rate 1.0 always resolves keep; rate 0.0 always resolves drop — a
        // second, contradicting call for the same trace must not change the
        // first answer.
        assert!(gate.resolve_outcome(trace, "ses-1", 1.0, true));
        assert!(
            gate.resolve_outcome(trace, "ses-1", 0.0, true),
            "the first resolution for a trace wins over a later, disagreeing one"
        );
        assert_eq!(gate.cached_outcome(trace), Some(true));
    }

    #[test]
    fn cached_outcome_is_none_for_an_unresolved_trace() {
        let gate = Arc::new(SamplingGate::new());
        let trace = TraceId::from_hex("0af7651916cd43dd8448eb211c80319c").unwrap();
        assert_eq!(gate.cached_outcome(trace), None);
    }

    #[test]
    fn outcome_overrides_are_bounded() {
        let gate = Arc::new(SamplingGate::new());
        for i in 0..MAX_OUTCOME_OVERRIDES as u128 + 100 {
            gate.resolve_outcome(TraceId::from(i + 1), "k", 1.0, false);
        }
        assert!(
            gate.outcome_overrides.values.len() <= MAX_OUTCOME_OVERRIDES,
            "the outcome-override map must stay bounded"
        );
    }
    fn small_history_gate(capacity: usize) -> Arc<SamplingGate> {
        let mut gate = SamplingGate::new();
        gate.outcome_overrides = TraceHistory::new(capacity);
        gate.force_kept = TraceHistory::new(capacity);
        Arc::new(gate)
    }

    #[test]
    fn saturated_history_admits_new_decisions_with_bounded_expiry_work() {
        let cache = TraceHistory::new(128);
        let old = Instant::now() - FORCE_KEEP_TTL - Duration::from_secs(1);
        for i in 1..=128u128 {
            assert_eq!(cache.insert_at(i.to_be_bytes(), true, old).1, 0);
        }
        for i in 129..=1_000u128 {
            let (keep, work) = cache.insert_at(i.to_be_bytes(), false, Instant::now());
            assert!(!keep);
            assert!(work <= TraceHistory::EXPIRY_BATCH);
            assert!(cache.values.len() <= 128);
            assert!(cache.order.lock().unwrap().len() <= 128);
            assert_eq!(cache.get(i.to_be_bytes()), Some(false));
        }
        assert!(cache.get(1u128.to_be_bytes()).is_none());
    }

    #[test]
    fn active_outcomes_survive_history_capacity_and_expiration_and_release_on_cancel() {
        let gate = Arc::new(SamplingGate::new());
        let trace = TraceId::from(1u128);
        let cx = gate.with_trace_decision(Context::new(), trace);
        assert!(gate.resolve_outcome(trace, "first", 1.0, false));
        gate.outcome_overrides
            .values
            .get_mut(&trace.to_bytes())
            .unwrap()
            .0 = Instant::now() - FORCE_KEEP_TTL;
        assert_eq!(gate.cached_outcome(trace), Some(true));
        for i in 2..=MAX_OUTCOME_OVERRIDES as u128 + 200 {
            assert!(!gate.resolve_outcome(TraceId::from(i), "other", 0.0, false));
        }
        assert!(gate.outcome_overrides.get(trace.to_bytes()).is_none());
        assert!(gate.resolve_outcome(trace, "contradiction", 0.0, false));
        assert_eq!(gate.active.len(), 1);
        drop(cx); // cancellation with no SDK span ever built
        assert!(gate.active.is_empty());
        assert_eq!(gate.cached_outcome(trace), None);
    }

    #[test]
    fn live_requests_with_the_same_remote_trace_share_one_first_writer() {
        use std::sync::Barrier;

        let gate = small_history_gate(1);
        let trace = TraceId::from(42u128);
        let barrier = Barrier::new(16);
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..16)
                .map(|i| {
                    let gate = &gate;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        let cx = gate.with_trace_decision(Context::new(), trace);
                        barrier.wait();
                        let answer = gate.resolve_outcome(trace, "shared", (i % 2) as f64, false);
                        // Evict history while every request still owns its cell.
                        gate.resolve_outcome(TraceId::from(100u128 + i), "other", 1.0, false);
                        barrier.wait();
                        assert_eq!(gate.cached_outcome(trace), Some(answer));
                        drop(cx);
                        answer
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(results.iter().all(|result| *result == results[0]));
        assert!(gate.active.is_empty());
    }

    #[test]
    fn outcome_resolution_uses_the_current_conversation_floor() {
        let gate = small_history_gate(1);
        let trace = TraceId::from(1u128);
        let cx = gate.with_trace_decision(Context::new(), trace);
        gate.install(Some(SamplingPolicy {
            enabled: true,
            min_conversation_rate: 1.0,
            ..SamplingPolicy::default()
        }));
        assert!(gate.resolve_outcome(trace, "session", 0.0, true));
        assert!(!gate.resolve_outcome(TraceId::from(2u128), "trace", 0.0, false));
        gate.install(Some(SamplingPolicy {
            enabled: true,
            min_conversation_rate: 0.0,
            ..SamplingPolicy::default()
        }));
        assert!(!gate.resolve_outcome(TraceId::from(3u128), "session", 0.0, true));
        assert!(gate.resolve_outcome(trace, "already resolved", 0.0, true));
        drop(cx);
        assert!(gate.active.is_empty());
    }

    #[test]
    fn history_decision_is_pinned_by_a_new_active_request() {
        let gate = small_history_gate(1);
        let trace = TraceId::from(1u128);
        assert!(gate.resolve_outcome(trace, "old", 1.0, false));
        let cx = gate.with_trace_decision(Context::new(), trace);
        assert!(gate.resolve_outcome(trace, "new", 0.0, false));
        gate.resolve_outcome(TraceId::from(2u128), "evict", 0.0, false);
        assert_eq!(gate.cached_outcome(trace), Some(true));
        drop(cx);
        assert!(gate.active.is_empty());
    }

    #[test]
    fn both_processors_pin_outcomes_until_the_last_participating_span_ends() {
        use opentelemetry::trace::{
            Span as _, SpanContext, TraceContextExt, Tracer, TracerProvider,
        };
        use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
        use opentelemetry_sdk::trace::{SimpleSpanProcessor, TracerProvider as SdkProvider};

        for (root_ends_first, resolve_first) in [(false, true), (true, true), (true, false)] {
            let gate = small_history_gate(1);
            let primary = InMemorySpanExporter::default();
            let secondary = InMemorySpanExporter::default();
            let provider = SdkProvider::builder()
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
            let tracer = provider.tracer("active-outcomes");
            let trace = TraceId::from(1u128);
            let cx = Context::new().with_remote_span_context(SpanContext::new(
                trace,
                SpanId::from(1u64),
                opentelemetry::trace::TraceFlags::SAMPLED,
                true,
                Default::default(),
            ));
            let cx = gate.with_trace_decision(
                with_sampling_inputs(
                    cx,
                    None,
                    false,
                    Some(RequestSampling {
                        keep: false,
                        log_content: false,
                    }),
                ),
                trace,
            );
            let mut root = tracer.start_with_context("http", &cx);
            let mut child = tracer.start_with_context("llm", &cx);
            drop(cx); // processors now own both span references
            if resolve_first {
                gate.resolve_outcome(trace, "response", 1.0, false);
            }
            if root_ends_first {
                root.end();
            } else {
                child.end();
            }
            if !resolve_first {
                gate.resolve_outcome(trace, "late response", 1.0, false);
            }
            gate.resolve_outcome(TraceId::from(2u128), "evict", 0.0, false);
            assert!(gate.outcome_overrides.get(trace.to_bytes()).is_none());
            assert_eq!(gate.active.len(), 1);
            if root_ends_first {
                drop(child); // cancellation/SDK Drop balances registration too
            } else {
                root.end();
            }
            assert!(
                gate.active.is_empty(),
                "both export processors must release their refs"
            );
            let expected = if resolve_first { 2 } else { 1 };
            // An HTTP span ending before the outcome is deliberately not
            // buffered/resurrected. Both destinations agree on later spans.
            assert_eq!(primary.get_finished_spans().unwrap().len(), expected);
            assert_eq!(secondary.get_finished_spans().unwrap().len(), expected);
            provider.shutdown().unwrap();
        }
    }

    #[test]
    fn active_force_keep_survives_history_eviction_and_wins_over_drop() {
        let gate = small_history_gate(1);
        let trace = TraceId::from(1u128);
        let cx = gate.with_trace_decision(Context::new(), trace);
        assert!(!gate.resolve_outcome(trace, "drop", 0.0, false));
        gate.force_keep_trace(trace);
        gate.force_keep_trace(TraceId::from(2u128));
        assert!(gate.force_kept.get(trace.to_bytes()).is_none());
        assert!(gate.is_force_kept(trace));
        drop(cx);
        assert!(gate.active.is_empty());
    }
}
