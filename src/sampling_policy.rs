//! Trace sampling policy — the admin-editable half of trace sampling.
//!
//! Mirrors [`crate::judge_policy`]: one env-scoped policy, a store trait with
//! an in-memory and a Postgres implementation, persisted as a single JSONB row.
//!
//! Ownership split: everything an operator edits in the UI lives here — the
//! master `enabled` switch, the `default_rate`, the ordered match rules, and
//! the conversation rate floor. The *static* OpenTelemetry sampler wiring
//! (`OTEL_TRACES_SAMPLER` / `OTEL_TRACES_SAMPLER_ARG`) stays in the
//! environment and still governs every non-LLM route; this policy governs only
//! the router's LLM frontends, where [`crate::telemetry::sampling`] resolves the
//! rate against a request's own tag and key, and can still keep a trace on how
//! the request turned out.
//!
//! Rates are chosen by an **ordered list of match rules**: the first rule whose
//! one selected target matches the request supplies the rate, and
//! `default_rate` catches everything no rule matched. Order *is* the precedence,
//! so an operator reorders rules instead of reasoning about which kind of
//! condition outranks which — and there is no second mechanism to reconcile,
//! because a rate for one API key is just a rule with `key_id` set.
//!
//! Every rule must choose exactly one target ([`SamplingPolicy::validate`]); an
//! absent target would match everything, so `default_rate` remains the only
//! catch-all.
//!
//! Rules live in this document rather than beside the things they name.
//! `chalk_router.usage_pools` carries only `id` / `name` / `created_at`, and a
//! per-key rate on `chalk_router.api_keys.permission` would be a parallel
//! mechanism whose precedence against this list an operator would have to learn.
//! One env-scoped row holds the whole decision.
//!
//! The policy document:
//!
//! ```json
//! {
//!   "enabled": true,
//!   "default_rate": 0.05,
//!   "default_log_content": true,
//!   "rules": [
//!     { "request_tag": "moderation_check", "rate": 1.0, "log_content": true },
//!     { "key_id": "ck-1a2b3c4d", "rate": 0.01, "log_content": false },
//!     { "usage_pool_id": "pool-internal", "rate": 0.5, "log_content": true }
//!   ],
//!   "min_conversation_rate": 0.0
//! }
//! ```
//!
//! There, `moderation_check` calls are always kept whoever makes them; anything
//! else from `ck-1a2b3c4d` is kept at 1% and recorded without prompt or
//! completion text; anything else from a key in `pool-internal` at 50%;
//! everything else at `default_rate`.
//!
//! The matched rule answers two independent questions: `rate`, whether the trace
//! is exported, and `log_content`, whether the spans carry the conversation
//! itself. One rule match settles both, so they can never be read off two
//! different rules.
//!
//! Two things sit outside the rules entirely. A request that returns an **error**
//! is always exported whatever `rate` said — a failure is the trace an operator
//! came looking for — and the `ROUTER_TRACE_LOG_CONTENT=false` kill switch
//! overrides `log_content` for the whole process.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::revisioned_policy::{
    RevisionedPolicy, RevisionedPolicyStore, RevisionedPolicyStoreError,
};

/// Rate used whenever no policy can be consulted: keep everything.
///
/// A trace that was never exported cannot be recovered, so every failure mode
/// on the read path (no store wired yet, a Postgres read error, a policy
/// document that decodes but carries a non-finite rate) resolves to this
/// instead of to a computed rate. Failing open costs export volume; failing
/// closed silently loses the traces an operator is looking at.
pub const FAIL_OPEN_RATE: f64 = 1.0;

/// The rate a policy with no explicit `default_rate` falls back to.
///
/// Keeping everything, not dropping everything: `enabled: true` with no rate
/// named is an incomplete edit, and the safe reading of an incomplete edit is
/// "sampling is on but nothing is being thinned yet".
fn default_rate_default() -> f64 {
    1.0
}

/// Omitted policy/rule content settings do not opt in to capturing bodies.
fn default_log_content_default() -> bool {
    false
}

/// Env-scoped trace sampling policy. `Default` is the safe off state
/// (`enabled: false`), under which every LLM-frontend span is kept and the
/// deployment behaves exactly as it did before this policy existed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SamplingPolicy {
    /// Monotonically increasing, store-managed version used to protect
    /// read-modify-write admin updates. Persisted in the JSON blob but
    /// deliberately not exposed in the admin wire format.
    #[serde(default)]
    pub revision: u64,
    /// Master switch. While false, no span is ever dropped by this policy.
    #[serde(default)]
    pub enabled: bool,
    /// Keep rate in `0.0..=1.0` for a request that matches no rule.
    #[serde(default = "default_rate_default")]
    pub default_rate: f64,
    /// Whether prompt and completion *content* is recorded for a request that
    /// matches no rule. Defaults to true, so an incomplete edit records what the
    /// deployment recorded before the policy existed; the
    /// `ROUTER_TRACE_LOG_CONTENT` kill switch still overrides it.
    #[serde(default = "default_log_content_default")]
    pub default_log_content: bool,
    /// Match rules in precedence order: the first one whose selected target
    /// matches a request supplies its rate. A request matching none of them is
    /// kept at `default_rate`.
    #[serde(default)]
    pub rules: Vec<SamplingRule>,
    /// Floor on the effective rate for a span that is sampled per
    /// *conversation* (one carrying a `session.id`). A conversation is the unit
    /// an operator reads, and it is the unit this floor protects: raising it
    /// guarantees a minimum share of whole conversations survives even when the
    /// per-tag or default rate is tuned very low for individual requests.
    #[serde(default)]
    pub min_conversation_rate: f64,
    /// Response-content rules, checked once the response is known and applied
    /// in place of the request-time rate for that trace — the first matching
    /// rule's `rate` wins, same precedence model as `rules`. Unlike `rules`
    /// this can override the request-time decision in either direction (keep
    /// a low-rate default's drop, or thin a high-rate default's keep), because
    /// it fires from what the request turned out to produce rather than from
    /// facts known before dispatch. See [`crate::telemetry::sampling::SamplingGate::resolve_outcome`].
    #[serde(default)]
    pub outcome_rules: Vec<OutcomeRule>,
}

impl Default for SamplingPolicy {
    fn default() -> Self {
        Self {
            revision: 0,
            enabled: false,
            default_rate: default_rate_default(),
            default_log_content: default_log_content_default(),
            rules: Vec::new(),
            min_conversation_rate: 0.0,
            outcome_rules: Vec::new(),
        }
    }
}

/// One rule of [`SamplingPolicy::rules`]: one request attribute it must match
/// and the keep rate it gets when it does. Which rule wins is decided by
/// position in the list, not by which target it names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SamplingRule {
    /// Exact match against the caller-supplied `x-chalk-request-tag`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_tag: Option<String>,
    /// Exact match against the caller's resolved identity — the API key prefix,
    /// or `user:<sub>` for a federated JWT caller. The same value that lands on
    /// a span as `chalk.caller.identity`, so a rule can be written from what an
    /// operator reads in a trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    /// Exact match against the calling key's usage pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_pool_id: Option<String>,
    /// Keep rate in `0.0..=1.0` for requests this rule matches.
    pub rate: f64,
    /// Whether prompt and completion content is recorded for requests this rule
    /// matches. Orthogonal to `rate`: a trace kept for its error can still be
    /// recorded without content, and a content-logged trace can still be thinned
    /// away. Defaults to true when a stored rule omits it.
    #[serde(default = "default_log_content_default")]
    pub log_content: bool,
}

impl SamplingRule {
    /// Whether this rule's target equals the corresponding request attribute.
    pub fn matches(&self, ctx: &MatchContext) -> bool {
        match (&self.request_tag, &self.key_id, &self.usage_pool_id) {
            (Some(value), None, None) => ctx.request_tag.as_deref() == Some(value),
            (None, Some(value), None) => ctx.key_id.as_deref() == Some(value),
            (None, None, Some(value)) => ctx.usage_pool_id.as_deref() == Some(value),
            _ => false,
        }
    }

    fn target_count(&self) -> usize {
        usize::from(self.request_tag.is_some())
            + usize::from(self.key_id.is_some())
            + usize::from(self.usage_pool_id.is_some())
    }
}

/// A basic scalar test against a response-body field. `Gt`/`Lt`/`Gte`/`Lte`
/// only apply when both the resolved field and [`OutcomeRule::value`] parse as
/// a number ([`serde_json::Value::as_f64`]); anything else is "no match," not
/// an error — the same fail-open spirit as an unusable rate elsewhere in this
/// file, applied here to an unusable comparison instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeComparator {
    Eq,
    Ne,
    Gt,
    Lt,
    Gte,
    Lte,
}

/// One response-content rule of [`SamplingPolicy::outcome_rules`].
///
/// `path` is a small dotted path into the response body — each segment is
/// either an object key or, if it parses as an integer, an array index (e.g.
/// `"choices.0.message.content"`) — deliberately not full JSONPath: no
/// wildcards, no bracket syntax, no filters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeRule {
    pub path: String,
    pub op: OutcomeComparator,
    pub value: serde_json::Value,
    /// Keep rate applied when this rule matches, in place of the request-time
    /// decision for the whole trace.
    pub rate: f64,
}

/// Outcome comparisons intentionally accept only JSON scalars. Structured
/// values are ambiguous with this dotted-path language and, more importantly,
/// must never be silently coerced into a different scalar when crossing an
/// admin API boundary.
pub fn validate_outcome_value(value: &serde_json::Value) -> Result<(), &'static str> {
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::String(_) => {
            Ok(())
        }
        serde_json::Value::Number(number) if number.as_f64().is_some_and(f64::is_finite) => Ok(()),
        serde_json::Value::Number(_) => Err("must be a finite number"),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            Err("must be null, a string, a finite number, or a boolean")
        }
    }
}

impl OutcomeRule {
    /// Whether `response` matches this rule's `path`/`op`/`value`.
    fn matches(&self, response: &serde_json::Value) -> bool {
        let Some(actual) = resolve_path(response, &self.path) else {
            return false;
        };
        self.matches_value(actual)
    }

    fn matches_string(&self, actual: &str) -> bool {
        match self.op {
            OutcomeComparator::Eq => self.value.as_str() == Some(actual),
            OutcomeComparator::Ne => self.value.as_str() != Some(actual),
            _ => false,
        }
    }

    fn matches_value(&self, actual: &serde_json::Value) -> bool {
        match self.op {
            OutcomeComparator::Eq => values_equal(actual, &self.value),
            OutcomeComparator::Ne => !values_equal(actual, &self.value),
            OutcomeComparator::Gt
            | OutcomeComparator::Lt
            | OutcomeComparator::Gte
            | OutcomeComparator::Lte => {
                match (actual.as_f64(), self.value.as_f64()) {
                    (Some(a), Some(b)) => match self.op {
                        OutcomeComparator::Gt => a > b,
                        OutcomeComparator::Lt => a < b,
                        OutcomeComparator::Gte => a >= b,
                        OutcomeComparator::Lte => a <= b,
                        OutcomeComparator::Eq | OutcomeComparator::Ne => unreachable!(),
                    },
                    // A non-numeric operand can't satisfy a numeric comparison;
                    // that is a rule the operator should fix, not a match.
                    _ => false,
                }
            }
        }
    }
}

/// Equality for [`OutcomeComparator::Eq`]/`Ne`, numeric-aware: `serde_json`'s
/// derived `PartialEq` distinguishes a `Number`'s internal representation, so
/// the integer `100` parsed from a response body and the float `100.0` every
/// number takes coming off the gRPC wire (`google.protobuf.Value` has no
/// integer variant) compare unequal by `==` despite being the same number.
/// That would break `Eq` against exactly the numbers this feature exists to
/// check, so two numbers compare by value here regardless of representation;
/// everything else keeps `serde_json`'s structural equality.
fn values_equal(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(a), Some(b)) => a == b,
        _ => a == b,
    }
}

/// Resolve a dotted path into `value` (see [`OutcomeRule::path`]).
fn resolve_path<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    path.split('.').try_fold(value, |current, segment| {
        if let Ok(index) = segment.parse::<usize>() {
            current.get(index)
        } else {
            current.get(segment)
        }
    })
}

/// The first matching [`OutcomeRule::rate`], or `None` if none of `rules`
/// matches `response`. Pure and response-only: the floor
/// (`min_conversation_rate`) is applied by the caller, which alone knows
/// whether this trace is conversation-keyed.
pub fn evaluate_outcome_rules(rules: &[OutcomeRule], response: &serde_json::Value) -> Option<f64> {
    rules
        .iter()
        .find(|rule| rule.matches(response))
        .map(|rule| rule.rate)
}

/// Paths compiled when a cached policy is installed, never per request.
/// Only fields with the exact same presence semantics as serialized Chat are
/// specialized; all other paths retain the generic JSON evaluator.
#[derive(Debug)]
pub(crate) struct CompiledChatOutcomeRules {
    selectors: Vec<ChatOutcomeSelector>,
    needs_content: bool,
}

#[derive(Debug)]
enum ChatOutcomeSelector {
    FinishReason(usize),
    Index(usize),
    Metadata(ChatMetadataField),
    Json(Vec<PathSegment>),
}

#[derive(Debug)]
enum ChatMetadataField {
    Id,
    Object,
    Created,
    Model,
    PromptTokens,
    CompletionTokens,
    TotalTokens,
}

impl ChatMetadataField {
    fn matches(&self, rule: &OutcomeRule, response: &crate::types::ChatCompletionResponse) -> bool {
        match self {
            Self::Id => rule.matches_string(&response.id),
            Self::Object => rule.matches_string(&response.object),
            Self::Model => rule.matches_string(&response.model),
            Self::Created => rule.matches_value(&response.created.into()),
            Self::PromptTokens | Self::CompletionTokens | Self::TotalTokens => {
                let Some(usage) = &response.usage else {
                    return false;
                };
                let number = match self {
                    Self::PromptTokens => Some(usage.prompt_tokens),
                    Self::CompletionTokens => usage.completion_tokens,
                    Self::TotalTokens => Some(usage.total_tokens),
                    _ => unreachable!(),
                };
                number.is_some_and(|number| rule.matches_value(&number.into()))
            }
        }
    }
}

#[derive(Debug)]
enum PathSegment {
    Index(usize),
    Key(String),
}

impl CompiledChatOutcomeRules {
    pub(crate) fn new(rules: &[OutcomeRule]) -> Self {
        let selectors: Vec<_> = rules
            .iter()
            .map(|rule| {
                let segments: Vec<_> = rule.path.split('.').collect();
                match segments.as_slice() {
                    ["choices", index, field] if index.parse::<usize>().is_ok() => {
                        let index = index.parse().expect("checked index");
                        match *field {
                            "finish_reason" => ChatOutcomeSelector::FinishReason(index),
                            "index" => ChatOutcomeSelector::Index(index),
                            _ => Self::json_selector(&segments),
                        }
                    }
                    ["id"] => ChatOutcomeSelector::Metadata(ChatMetadataField::Id),
                    ["object"] => ChatOutcomeSelector::Metadata(ChatMetadataField::Object),
                    ["created"] => ChatOutcomeSelector::Metadata(ChatMetadataField::Created),
                    ["model"] => ChatOutcomeSelector::Metadata(ChatMetadataField::Model),
                    ["usage", "prompt_tokens"] => {
                        ChatOutcomeSelector::Metadata(ChatMetadataField::PromptTokens)
                    }
                    ["usage", "completion_tokens"] => {
                        ChatOutcomeSelector::Metadata(ChatMetadataField::CompletionTokens)
                    }
                    ["usage", "total_tokens"] => {
                        ChatOutcomeSelector::Metadata(ChatMetadataField::TotalTokens)
                    }
                    _ => Self::json_selector(&segments),
                }
            })
            .collect();
        let needs_content = selectors
            .iter()
            .any(|selector| matches!(selector, ChatOutcomeSelector::Json(_)));
        Self {
            selectors,
            needs_content,
        }
    }

    fn json_selector(segments: &[&str]) -> ChatOutcomeSelector {
        ChatOutcomeSelector::Json(
            segments
                .iter()
                .map(|segment| match segment.parse::<usize>() {
                    Ok(index) => PathSegment::Index(index),
                    Err(_) => PathSegment::Key((*segment).to_owned()),
                })
                .collect(),
        )
    }

    pub(crate) fn needs_content(&self) -> bool {
        self.needs_content
    }

    pub(crate) fn matched_rate(
        &self,
        rules: &[OutcomeRule],
        choices: &[crate::types::ChatChoice],
        completion: Option<&crate::types::ChatCompletionResponse>,
    ) -> Option<f64> {
        // Build the generic response at most once, and only if evaluation
        // reaches an arbitrary selector. First-match ordering is unchanged.
        let mut json = None;
        for (rule, selector) in rules.iter().zip(&self.selectors) {
            let matched = match selector {
                ChatOutcomeSelector::FinishReason(index) => {
                    choices.get(*index).is_some_and(|choice| {
                        match choice.finish_reason.as_deref() {
                            Some(reason) => rule.matches_string(reason),
                            None => rule.matches_value(&serde_json::Value::Null),
                        }
                    })
                }
                ChatOutcomeSelector::Index(index) => choices.get(*index).is_some_and(|choice| {
                    rule.matches_value(&serde_json::Value::from(choice.index))
                }),
                ChatOutcomeSelector::Metadata(field) => {
                    completion.is_some_and(|response| field.matches(rule, response))
                }
                ChatOutcomeSelector::Json(path) => {
                    if json.is_none() {
                        json = Some(match completion {
                            Some(completion) => serde_json::to_value(completion).ok()?,
                            None => serde_json::json!({ "choices": choices }),
                        });
                    }
                    path.iter()
                        .try_fold(
                            json.as_ref().expect("initialized response"),
                            |value, segment| match segment {
                                PathSegment::Index(index) => value.get(*index),
                                PathSegment::Key(key) => value.get(key),
                            },
                        )
                        .is_some_and(|actual| rule.matches_value(actual))
                }
            };
            if matched {
                return Some(rule.rate);
            }
        }
        None
    }
}

/// What a request is matched on: resolved once, at request entry, where the
/// header and the authenticated key are both in hand.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MatchContext {
    pub request_tag: Option<String>,
    pub key_id: Option<String>,
    pub usage_pool_id: Option<String>,
}

/// What the policy says about one request.
///
/// The two answers are independent. `rate` decides whether the trace is
/// exported at all — and an error response overrides it entirely, see
/// [`crate::telemetry::sampling::SamplingGate::force_keep_trace`] — while
/// `log_content` decides whether the spans that *are* exported carry the
/// conversation itself.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedSampling {
    pub rate: f64,
    pub log_content: bool,
}

/// Whether a rate is a usable probability: finite and inside `0.0..=1.0`.
pub fn rate_usable(rate: f64) -> bool {
    rate.is_finite() && (0.0..=1.0).contains(&rate)
}

impl SamplingPolicy {
    /// Everything one request needs from the policy, resolved in a single pass
    /// over [`Self::rules`] so the rate and the content setting can never come
    /// from two different rules.
    ///
    /// A disabled policy resolves to [`FAIL_OPEN_RATE`] and to content logging
    /// left as the environment has it, so the off state behaves exactly as a
    /// deployment without this policy.
    pub fn resolve(&self, ctx: &MatchContext, conversation_keyed: bool) -> ResolvedSampling {
        if !self.enabled {
            return ResolvedSampling {
                rate: FAIL_OPEN_RATE,
                log_content: true,
            };
        }
        let matched = self.rules.iter().find(|rule| rule.matches(ctx));
        let (base, log_content) = match matched {
            Some(rule) => (rule.rate, rule.log_content),
            None => (self.default_rate, self.default_log_content),
        };
        ResolvedSampling {
            rate: self.floored_rate(base, conversation_keyed),
            log_content,
        }
    }

    /// The keep rate for one request: the first matching rule's rate, or
    /// `default_rate`.
    ///
    /// `conversation_keyed` says whether the caller will hash a `session.id`
    /// rather than a bare trace id, which is what makes `min_conversation_rate`
    /// applicable; the floor is applied after a rule (or the default) has
    /// picked a rate.
    ///
    /// A disabled policy — and any rate that survived into storage without
    /// being a usable probability — resolves to [`FAIL_OPEN_RATE`], so a
    /// malformed document can only ever export too much, never too little.
    pub fn effective_rate(&self, ctx: &MatchContext, conversation_keyed: bool) -> f64 {
        self.resolve(ctx, conversation_keyed).rate
    }

    /// Apply `min_conversation_rate` to a rate a rule or the default supplied.
    /// `pub(crate)` so [`crate::telemetry::sampling::SamplingGate`] can apply
    /// the same floor to a matched [`OutcomeRule`]'s rate.
    pub(crate) fn floored_rate(&self, base: f64, conversation_keyed: bool) -> f64 {
        if !rate_usable(base) {
            return FAIL_OPEN_RATE;
        }
        let floor = if conversation_keyed && rate_usable(self.min_conversation_rate) {
            self.min_conversation_rate
        } else {
            0.0
        };
        base.max(floor)
    }

    /// The first matching [`OutcomeRule`]'s rate, or `None` if the policy is
    /// disabled, has no outcome rules, or none matches `response`. Raw —
    /// unfloored — because whether this trace is conversation-keyed is not
    /// known here; see [`crate::telemetry::sampling::SamplingGate::resolve_outcome`],
    /// which applies [`Self::floored_rate`] once it knows.
    pub fn matched_outcome_rate(&self, response: &serde_json::Value) -> Option<f64> {
        if !self.enabled {
            return None;
        }
        evaluate_outcome_rules(&self.outcome_rules, response)
    }

    /// Validate admin input before persisting. Returns the first problem as a
    /// human-readable message; the service layer maps it to invalid-argument on
    /// both wire formats. Rates are checked here because a stored `NaN` or
    /// out-of-range rate can only be interpreted by failing open, which
    /// silently ignores the operator's intent.
    pub fn validate(&self) -> Result<(), String> {
        if !rate_usable(self.default_rate) {
            return Err(format!(
                "default_rate must be within 0.0..=1.0; got {}",
                self.default_rate
            ));
        }
        if !rate_usable(self.min_conversation_rate) {
            return Err(format!(
                "min_conversation_rate must be within 0.0..=1.0; got {}",
                self.min_conversation_rate
            ));
        }
        for (index, rule) in self.rules.iter().enumerate() {
            if rule.target_count() != 1 {
                return Err(format!(
                    "rules[{index}] must set exactly one of request_tag, key_id or usage_pool_id"
                ));
            }
            if !rate_usable(rule.rate) {
                return Err(format!(
                    "rules[{index}].rate must be within 0.0..=1.0; got {}",
                    rule.rate
                ));
            }
        }
        for (index, rule) in self.outcome_rules.iter().enumerate() {
            if rule.path.trim().is_empty() {
                return Err(format!("outcome_rules[{index}].path must be non-empty"));
            }
            if !rate_usable(rule.rate) {
                return Err(format!(
                    "outcome_rules[{index}].rate must be within 0.0..=1.0; got {}",
                    rule.rate
                ));
            }
            validate_outcome_value(&rule.value)
                .map_err(|reason| format!("outcome_rules[{index}].value {reason}"))?;
        }
        Ok(())
    }
}

impl RevisionedPolicy for SamplingPolicy {
    const CACHE_TAG: &'static str = "sampling_policy";
    const DISPLAY_NAME: &'static str = "sampling policy";

    fn revision(&self) -> u64 {
        self.revision
    }

    fn set_revision(&mut self, revision: u64) {
        self.revision = revision;
    }
}

// ---------------------------------------------------------------------------
// Store trait and in-memory store: shared generic implementation (see
// crate::revisioned_policy). Unlike the judge and fallback policies there is
// no short-TTL cache wrapper here: the read that has to be cheap is the one
// inside the span processor, which is synchronous and therefore keeps its own
// snapshot (see [`crate::telemetry::sampling::SamplingGate`]). Wrapping this
// store as well would add a second, differently-expiring copy of the same
// document without removing a single query from the request path.
// ---------------------------------------------------------------------------

/// Alias so existing callers keep compiling unchanged; see
/// [`crate::revisioned_policy::RevisionedPolicyStoreError`].
pub type SamplingPolicyStoreError = RevisionedPolicyStoreError;

/// Alias for the shared in-memory store; see
/// [`crate::revisioned_policy::InMemoryRevisionedStore`].
pub type InMemorySamplingPolicyStore =
    crate::revisioned_policy::InMemoryRevisionedStore<SamplingPolicy>;

/// Marker for `Arc<dyn SamplingPolicyStore>` trait objects. The real trait,
/// [`RevisionedPolicyStore<SamplingPolicy>`], carries a generic parameter and
/// so cannot be spelled bare after `dyn`; this marker is blanket-implemented
/// for every type that implements the generic trait, so existing call sites
/// (service/gRPC layers constructing `Arc<dyn SamplingPolicyStore>`) need no
/// changes.
pub trait SamplingPolicyStore: RevisionedPolicyStore<SamplingPolicy> {}
impl<T: RevisionedPolicyStore<SamplingPolicy>> SamplingPolicyStore for T {}

// ---------------------------------------------------------------------------
// PostgreSQL implementation
// ---------------------------------------------------------------------------

/// One policy row per environment.
#[derive(Clone)]
pub struct PostgresSamplingPolicyStore {
    pool: sqlx::PgPool,
    /// Tenancy scope (see `PostgresApiKeyStore`).
    environment_id: String,
}

impl PostgresSamplingPolicyStore {
    pub fn new(pool: sqlx::PgPool, environment_id: String) -> Self {
        Self {
            pool,
            environment_id,
        }
    }
}

#[async_trait]
impl RevisionedPolicyStore<SamplingPolicy> for PostgresSamplingPolicyStore {
    async fn get(&self) -> Result<SamplingPolicy, SamplingPolicyStoreError> {
        let row: Option<(serde_json::Value,)> = sqlx::query_as(
            "SELECT policy FROM chalk_router.sampling_policy WHERE environment_id = $1",
        )
        .bind(&self.environment_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to read sampling policy");
            SamplingPolicyStoreError::other(format!("failed to read sampling policy: {e}"))
        })?;

        match row {
            Some((json,)) => serde_json::from_value(json).map_err(|e| {
                tracing::error!(error = %e, "Failed to decode sampling policy");
                SamplingPolicyStoreError::other(format!("failed to decode sampling policy: {e}"))
            }),
            None => Ok(SamplingPolicy::default()),
        }
    }

    async fn set(&self, policy: SamplingPolicy) -> Result<(), SamplingPolicyStoreError> {
        let expected = self.get().await?;
        self.set_cas(&expected, policy).await
    }

    async fn set_cas(
        &self,
        expected: &SamplingPolicy,
        mut policy: SamplingPolicy,
    ) -> Result<(), SamplingPolicyStoreError> {
        policy.revision = expected.revision.saturating_add(1);
        let json = serde_json::to_value(&policy).map_err(|e| {
            SamplingPolicyStoreError::other(format!("failed to encode sampling policy: {e}"))
        })?;
        let result = sqlx::query(
            r#"
            INSERT INTO chalk_router.sampling_policy (environment_id, policy) VALUES ($1, $2)
            ON CONFLICT (environment_id) DO UPDATE SET policy = EXCLUDED.policy
            WHERE COALESCE((chalk_router.sampling_policy.policy->>'revision')::bigint, 0) = $3
            "#,
        )
        .bind(&self.environment_id)
        .bind(&json)
        .bind(expected.revision as i64)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to persist sampling policy");
            SamplingPolicyStoreError::other(format!("failed to persist sampling policy: {e}"))
        })?;
        if result.rows_affected() == 0 {
            return Err(SamplingPolicyStoreError::Conflict);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_policy() -> SamplingPolicy {
        SamplingPolicy {
            enabled: true,
            default_rate: 0.05,
            ..SamplingPolicy::default()
        }
    }

    /// A rule stating one condition, which is the minimum a valid rule states.
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

    fn ctx(request_tag: Option<&str>, key_id: Option<&str>, pool: Option<&str>) -> MatchContext {
        MatchContext {
            request_tag: request_tag.map(str::to_string),
            key_id: key_id.map(str::to_string),
            usage_pool_id: pool.map(str::to_string),
        }
    }

    #[test]
    fn default_policy_keeps_everything() {
        let mut policy = SamplingPolicy::default();
        policy.rules.push(rule("request_tag", "anything", 0.0));
        assert!(!policy.enabled);
        assert_eq!(
            policy.effective_rate(&MatchContext::default(), false),
            FAIL_OPEN_RATE
        );
        assert_eq!(
            policy.effective_rate(&ctx(Some("anything"), Some("ck-1"), Some("pool-1")), true),
            FAIL_OPEN_RATE,
            "a disabled policy ignores every rule"
        );
    }

    #[test]
    fn an_empty_document_decodes_to_the_off_state() {
        let policy: SamplingPolicy = serde_json::from_str("{}").expect("empty object decodes");
        assert!(!policy.enabled);
        assert_eq!(policy.default_rate, 1.0);
        assert!(!policy.default_log_content);
        assert!(policy.rules.is_empty());
        assert_eq!(policy.revision, 0);
    }

    #[test]
    fn a_stored_rule_may_omit_log_content() {
        let rule: SamplingRule =
            serde_json::from_str(r#"{"request_tag":"t","rate":0.5}"#).expect("rule decodes");
        assert!(
            !rule.log_content,
            "an omitted log_content must not authorize recording content"
        );
        assert_eq!(rule.request_tag.as_deref(), Some("t"));
        assert!(rule.key_id.is_none());
    }

    #[test]
    fn one_rule_match_settles_both_the_rate_and_content_logging() {
        let mut policy = enabled_policy();
        policy.default_log_content = true;
        policy.rules = vec![
            SamplingRule {
                request_tag: None,
                key_id: Some("ck-quiet".to_string()),
                usage_pool_id: None,
                rate: 1.0,
                log_content: false,
            },
            rule("key_id", "ck-loud", 0.0),
        ];

        // Kept, but without the conversation.
        let quiet = policy.resolve(&ctx(None, Some("ck-quiet"), None), false);
        assert_eq!(quiet.rate, 1.0);
        assert!(!quiet.log_content);

        // Dropped, though its rule would have logged content had it survived —
        // the two answers are independent.
        let loud = policy.resolve(&ctx(None, Some("ck-loud"), None), false);
        assert_eq!(loud.rate, 0.0);
        assert!(loud.log_content);

        // No rule matched: both fall back to the defaults.
        let other = policy.resolve(&ctx(None, Some("ck-other"), None), false);
        assert_eq!(other.rate, 0.05);
        assert!(other.log_content);
    }

    #[test]
    fn a_disabled_policy_leaves_content_logging_to_the_environment() {
        let resolved = SamplingPolicy::default().resolve(&MatchContext::default(), false);
        assert_eq!(resolved.rate, FAIL_OPEN_RATE);
        assert!(
            resolved.log_content,
            "the off state must not start suppressing content"
        );
    }

    #[test]
    fn the_first_matching_rule_wins_and_order_is_the_precedence() {
        let tag_rule = rule("request_tag", "moderation_check", 1.0);
        let key_rule = rule("key_id", "ck-1a2b3c4d", 0.01);
        let request = ctx(Some("moderation_check"), Some("ck-1a2b3c4d"), None);

        // Both rules match this request; position alone decides which applies.
        let mut tag_first = enabled_policy();
        tag_first.rules = vec![tag_rule.clone(), key_rule.clone()];
        assert_eq!(tag_first.effective_rate(&request, false), 1.0);

        let mut key_first = enabled_policy();
        key_first.rules = vec![key_rule, tag_rule];
        assert_eq!(
            key_first.effective_rate(&request, false),
            0.01,
            "swapping the rules swaps the outcome — nothing else decides it"
        );
    }

    #[test]
    fn a_request_matching_no_rule_falls_through_to_the_default() {
        let mut policy = enabled_policy();
        policy.rules = vec![
            rule("request_tag", "moderation_check", 1.0),
            rule("key_id", "ck-1a2b3c4d", 0.01),
            rule("usage_pool_id", "pool-internal", 0.5),
        ];

        assert_eq!(
            policy.effective_rate(
                &ctx(Some("other"), Some("ck-99"), Some("pool-other")),
                false
            ),
            0.05
        );
        assert_eq!(
            policy.effective_rate(&MatchContext::default(), false),
            0.05,
            "a request with none of the three facts matches no rule"
        );
        // Each condition still matches on its own.
        assert_eq!(
            policy.effective_rate(&ctx(None, None, Some("pool-internal")), false),
            0.5
        );
    }

    #[test]
    fn conversation_floor_applies_only_to_session_keyed_spans() {
        let mut policy = enabled_policy();
        policy.min_conversation_rate = 0.2;

        assert_eq!(policy.effective_rate(&MatchContext::default(), true), 0.2);
        assert_eq!(policy.effective_rate(&MatchContext::default(), false), 0.05);
        // The floor raises a matched rule's rate, never lowers it.
        policy.rules = vec![rule("key_id", "ck-1", 0.9)];
        assert_eq!(
            policy.effective_rate(&ctx(None, Some("ck-1"), None), true),
            0.9
        );
    }

    #[test]
    fn unusable_stored_rates_fail_open() {
        let mut policy = enabled_policy();
        policy.default_rate = f64::NAN;
        assert_eq!(
            policy.effective_rate(&MatchContext::default(), false),
            FAIL_OPEN_RATE
        );

        policy.default_rate = 0.05;
        policy.min_conversation_rate = f64::INFINITY;
        assert_eq!(
            policy.effective_rate(&MatchContext::default(), true),
            0.05,
            "an unusable floor is ignored rather than poisoning the rate"
        );

        // A rule that reached storage with an unusable rate keeps its trace
        // rather than dropping it on an unreadable number.
        policy.rules = vec![rule("key_id", "ck-bad", -1.0)];
        assert_eq!(
            policy.effective_rate(&ctx(None, Some("ck-bad"), None), false),
            FAIL_OPEN_RATE
        );
    }

    #[test]
    fn validate_rejects_rates_outside_the_unit_interval() {
        let mut policy = enabled_policy();
        assert!(policy.validate().is_ok());

        policy.default_rate = 1.5;
        assert!(policy.validate().is_err());

        policy.default_rate = 0.5;
        policy.min_conversation_rate = -0.1;
        assert!(policy.validate().is_err());

        policy.min_conversation_rate = 0.0;
        policy.rules = vec![rule("request_tag", "bad", 2.0)];
        assert!(policy.validate().is_err());
    }

    #[test]
    fn every_rule_requires_exactly_one_target() {
        let mut policy = enabled_policy();
        policy.rules = vec![SamplingRule {
            request_tag: None,
            key_id: None,
            usage_pool_id: None,
            rate: 0.0,
            log_content: true,
        }];

        let error = policy
            .validate()
            .expect_err("a rule without a target must be rejected");
        assert!(
            error.contains("rules[0]") && error.contains("exactly one"),
            "the message should say which rule and why: {error}"
        );

        // Any one target makes it valid.
        policy.rules[0].usage_pool_id = Some("pool-internal".to_string());
        assert!(policy.validate().is_ok());

        for (first, second) in [
            ("request_tag", "key_id"),
            ("request_tag", "usage_pool_id"),
            ("key_id", "usage_pool_id"),
        ] {
            let mut combined = enabled_policy();
            let mut rule = rule(first, "first", 0.5);
            match second {
                "request_tag" => rule.request_tag = Some("second".to_string()),
                "key_id" => rule.key_id = Some("second".to_string()),
                "usage_pool_id" => rule.usage_pool_id = Some("second".to_string()),
                _ => unreachable!(),
            }
            combined.rules = vec![rule];
            assert!(
                !combined.rules[0].matches(&ctx(Some("first"), Some("first"), Some("first"))),
                "combined rule targets cannot match"
            );
            let error = combined
                .validate()
                .expect_err("combined rule targets must be rejected");
            assert!(error.contains("exactly one"), "{first} + {second}: {error}");
        }
    }

    fn outcome_rule(
        path: &str,
        op: OutcomeComparator,
        value: serde_json::Value,
        rate: f64,
    ) -> OutcomeRule {
        OutcomeRule {
            path: path.to_string(),
            op,
            value,
            rate,
        }
    }

    #[test]
    fn outcome_rule_matches_an_object_key_and_an_array_index() {
        let response = serde_json::json!({
            "flag": true,
            "choices": [{"message": {"content": "hi"}}],
        });
        assert_eq!(
            evaluate_outcome_rules(
                &[outcome_rule(
                    "flag",
                    OutcomeComparator::Eq,
                    serde_json::json!(true),
                    1.0
                )],
                &response
            ),
            Some(1.0)
        );
        assert_eq!(
            evaluate_outcome_rules(
                &[outcome_rule(
                    "choices.0.message.content",
                    OutcomeComparator::Eq,
                    serde_json::json!("hi"),
                    0.5
                )],
                &response
            ),
            Some(0.5)
        );
    }

    #[test]
    fn outcome_rule_missing_path_does_not_match() {
        let response = serde_json::json!({"flag": true});
        assert_eq!(
            evaluate_outcome_rules(
                &[outcome_rule(
                    "nope",
                    OutcomeComparator::Eq,
                    serde_json::json!(true),
                    1.0
                )],
                &response
            ),
            None
        );
        assert_eq!(
            evaluate_outcome_rules(
                &[outcome_rule(
                    "choices.5.message",
                    OutcomeComparator::Eq,
                    serde_json::json!("x"),
                    1.0
                )],
                &response
            ),
            None
        );
    }

    #[test]
    fn outcome_rule_numeric_comparators() {
        let response = serde_json::json!({"score": 7});
        for (op, expected) in [
            (OutcomeComparator::Gt, true),
            (OutcomeComparator::Gte, true),
            (OutcomeComparator::Lt, false),
            (OutcomeComparator::Lte, false),
        ] {
            let matched = evaluate_outcome_rules(
                &[outcome_rule("score", op, serde_json::json!(5), 1.0)],
                &response,
            );
            assert_eq!(matched.is_some(), expected, "op {op:?}");
        }
    }

    #[test]
    fn outcome_rule_eq_matches_a_number_regardless_of_its_json_representation() {
        // A response body's `100` (parsed as an integer literal) and a rule
        // value of `100.0` (every number crossing the gRPC wire is a
        // `google.protobuf.Value` float, with no integer variant) are the
        // same number but different `serde_json::Value::Number`
        // representations internally — `==` must not tell them apart here.
        let response = serde_json::json!({"count": 100});
        let float_rule_value: serde_json::Value =
            serde_json::from_str("100.0").expect("float literal");
        assert!(
            matches!(&float_rule_value, serde_json::Value::Number(n) if n.is_f64()),
            "test setup must produce the Float representation"
        );
        assert_eq!(
            evaluate_outcome_rules(
                &[outcome_rule(
                    "count",
                    OutcomeComparator::Eq,
                    float_rule_value.clone(),
                    1.0
                )],
                &response
            ),
            Some(1.0),
            "100 (int) must equal 100.0 (float) under Eq"
        );
        assert_eq!(
            evaluate_outcome_rules(
                &[outcome_rule(
                    "count",
                    OutcomeComparator::Ne,
                    float_rule_value,
                    1.0
                )],
                &response
            ),
            None,
            "100 (int) must not satisfy Ne against 100.0 (float)"
        );
    }

    #[test]
    fn outcome_rule_numeric_comparator_on_non_numeric_field_fails_open_to_no_match() {
        let response = serde_json::json!({"flag": true});
        assert_eq!(
            evaluate_outcome_rules(
                &[outcome_rule(
                    "flag",
                    OutcomeComparator::Gt,
                    serde_json::json!(0),
                    1.0
                )],
                &response
            ),
            None,
            "a comparison that cannot be evaluated must not match, not error"
        );
    }

    #[test]
    fn outcome_rules_first_match_wins() {
        let response = serde_json::json!({"flag": true});
        let rules = vec![
            outcome_rule("flag", OutcomeComparator::Eq, serde_json::json!(true), 1.0),
            outcome_rule("flag", OutcomeComparator::Eq, serde_json::json!(true), 0.1),
        ];
        assert_eq!(evaluate_outcome_rules(&rules, &response), Some(1.0));
    }

    #[test]
    fn matched_outcome_rate_respects_the_master_switch() {
        let response = serde_json::json!({"flag": true});
        let mut policy = enabled_policy();
        policy.outcome_rules = vec![outcome_rule(
            "flag",
            OutcomeComparator::Eq,
            serde_json::json!(true),
            1.0,
        )];
        assert_eq!(policy.matched_outcome_rate(&response), Some(1.0));

        policy.enabled = false;
        assert_eq!(
            policy.matched_outcome_rate(&response),
            None,
            "a disabled policy must not apply outcome rules either"
        );
    }

    #[test]
    fn validate_rejects_bad_outcome_rules() {
        let mut policy = enabled_policy();
        assert!(policy.validate().is_ok());

        policy.outcome_rules = vec![outcome_rule(
            "",
            OutcomeComparator::Eq,
            serde_json::json!(true),
            1.0,
        )];
        assert!(policy.validate().is_err(), "an empty path must be rejected");

        policy.outcome_rules = vec![outcome_rule(
            "flag",
            OutcomeComparator::Eq,
            serde_json::json!(true),
            2.0,
        )];
        assert!(
            policy.validate().is_err(),
            "an out-of-range outcome rate must be rejected"
        );

        for value in [serde_json::json!([]), serde_json::json!({"nested": true})] {
            policy.outcome_rules = vec![outcome_rule("flag", OutcomeComparator::Eq, value, 1.0)];
            assert!(
                policy.validate().is_err(),
                "structured outcome values must be rejected"
            );
        }

        for value in [
            serde_json::Value::Null,
            serde_json::json!("complete"),
            serde_json::json!(1.5),
            serde_json::json!(false),
        ] {
            policy.outcome_rules = vec![outcome_rule("flag", OutcomeComparator::Eq, value, 1.0)];
            assert!(
                policy.validate().is_ok(),
                "scalar outcome value must be accepted"
            );
        }
    }

    #[tokio::test]
    async fn set_cas_rejects_a_stale_policy_revision() {
        let store = InMemorySamplingPolicyStore::new(SamplingPolicy::default());
        let base = store.get().await.unwrap();

        let mut first = base.clone();
        first.enabled = true;
        store
            .set_cas(&base, first)
            .await
            .expect("first write succeeds");

        let mut stale = base;
        stale.default_rate = 0.5;
        let err = store
            .set_cas(&stale, stale.clone())
            .await
            .expect_err("stale write must conflict");
        assert!(err.is_conflict());

        let current = store.get().await.unwrap();
        assert_eq!(current.revision, 1);
        assert!(current.enabled);
        assert_eq!(
            current.default_rate, 1.0,
            "stale write must not overwrite fields"
        );
    }
    #[test]
    fn compiled_chat_outcomes_match_generic_rules_for_metadata_and_missing_fields() {
        let mut response: crate::types::ChatCompletionResponse = serde_json::from_value(serde_json::json!({
            "id": "chat-1", "object": "chat.completion", "created": 123, "model": "test",
            "choices": [
                {"index": 7, "message": {"role": "assistant", "content": "answer"}, "finish_reason": "stop"},
                {"index": 9, "message": {"role": "assistant"}, "finish_reason": null}
            ],
            "usage": {"prompt_tokens": 12, "total_tokens": 15}
        })).unwrap();
        for with_usage in [true, false] {
            if !with_usage {
                response.usage = None;
            }
            let full = serde_json::to_value(&response).unwrap();
            let streamed = serde_json::json!({"choices": response.choices});
            for path in [
                "choices.0.finish_reason",
                "choices.00.finish_reason",
                "choices.1.finish_reason",
                "choices.9.finish_reason",
                "choices.0.index",
                "choices.1.index",
                "choices.9.index",
                "id",
                "model",
                "created",
                "object",
                "usage.prompt_tokens",
                "usage.total_tokens",
                "usage.completion_tokens",
                "choices.0.message.content",
                "choices.1.message.content",
                "choices.0.message.content.missing",
                "choices.-1.finish_reason",
                "choices.0.missing",
            ] {
                for op in [
                    OutcomeComparator::Eq,
                    OutcomeComparator::Ne,
                    OutcomeComparator::Gt,
                    OutcomeComparator::Gte,
                    OutcomeComparator::Lt,
                    OutcomeComparator::Lte,
                ] {
                    for value in [
                        serde_json::Value::Null,
                        serde_json::json!("stop"),
                        serde_json::json!("test"),
                        serde_json::json!(7),
                        serde_json::json!(7.0),
                        serde_json::json!(true),
                    ] {
                        let rules = [outcome_rule(path, op, value, 0.3)];
                        let compiled = CompiledChatOutcomeRules::new(&rules);
                        assert_eq!(
                            compiled.matched_rate(&rules, &response.choices, Some(&response)),
                            evaluate_outcome_rules(&rules, &full),
                            "full: {path} {op:?}"
                        );
                        assert_eq!(
                            compiled.matched_rate(&rules, &response.choices, None),
                            evaluate_outcome_rules(&rules, &streamed),
                            "stream: {path} {op:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn compiled_chat_outcomes_preserve_first_match_across_generic_and_metadata_rules() {
        let response: crate::types::ChatCompletionResponse = serde_json::from_value(serde_json::json!({
            "id": "chat-1", "object": "chat.completion", "created": 1, "model": "test",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "answer"}, "finish_reason": "stop"}]
        })).unwrap();
        let metadata = outcome_rule(
            "choices.0.finish_reason",
            OutcomeComparator::Eq,
            serde_json::json!("stop"),
            0.8,
        );
        let content = outcome_rule(
            "choices.0.message.content",
            OutcomeComparator::Eq,
            serde_json::json!("answer"),
            0.2,
        );
        assert!(!CompiledChatOutcomeRules::new(std::slice::from_ref(&metadata)).needs_content());
        for rules in [
            vec![metadata.clone(), content.clone()],
            vec![content, metadata],
        ] {
            let compiled = CompiledChatOutcomeRules::new(&rules);
            assert!(compiled.needs_content());
            assert_eq!(
                compiled.matched_rate(&rules, &response.choices, Some(&response)),
                Some(rules[0].rate)
            );
        }
    }
}
