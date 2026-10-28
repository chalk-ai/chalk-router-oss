//! Exact-prefix completion caching.
//!
//! A cache of finished model responses, addressed by a hash of the *exact*
//! request prefix that produced them. "Exact prefix" is literal: the digest
//! covers the whole conversation prefix (every message, in order, byte for
//! byte) plus every generation parameter that can change the answer. There is
//! no similarity search and no truncation — two requests share a cache entry
//! only when they are the same request. A conversation naturally re-sends its
//! prefix on every turn, so re-asking an identical question (a replayed
//! evaluation, a retried agent step, a shared system prompt answered the same
//! way) hits; a conversation that has advanced by one turn does not.
//!
//! ## Where it sits
//!
//! Above the provider backends, not inside them: the request path consults the
//! cache *after* the API key's permission gate and *before* the judge and
//! dispatch, and stores the canonical response afterwards. Nothing here knows
//! which provider served the request, so a `chalk/*` route that moves between
//! OpenAI, Bedrock, and vLLM keeps its cache — and so both the OpenAI-shaped
//! `/v1/chat/completions` surface and the Anthropic `/v1/messages`
//! passthrough use the same cache with the same key derivation.
//!
//! ## Streaming
//!
//! Both surfaces cache both shapes, but store different things, because they
//! sit at different levels:
//!
//! * **`/v1/chat/completions`** stores the complete native JSON reply. Plain
//!   text/tool-call replies can also be assembled from streams and replayed
//!   across modes; unfamiliar reply semantics bypass that conversion. Native
//!   non-streaming cache hits preserve the stored JSON bytes. Side-effecting
//!   or unfamiliar request controls bypass caching, while nested raw values
//!   and forwarded beta flags participate in the cache identity.
//! * **`/v1/messages`** is a byte passthrough, so it stores the upstream
//!   response verbatim (a JSON body, or a whole SSE stream) and replays it
//!   unchanged. `stream` is part of the raw body and therefore part of the
//!   digest, so the two shapes are separate entries — which they must be, since
//!   they are different stored bytes. Because it stores bytes rather than a
//!   parsed response, this surface also keys on the request headers that shape
//!   what upstream returns — see [`ResponseShapeHeaders`].
//!
//! Either way, only a *finished* generation is stored: a stream severed
//! mid-answer writes nothing, so a partial completion can never be replayed to
//! a later caller as a whole one.
//!
//! ## What the key does not cover
//!
//! The digest names the *requested* public model id, not the upstream the route
//! resolves to. Repointing a `chalk/*` route at a different upstream model — or
//! editing a judge ladder — therefore does not invalidate entries stored under
//! the old wiring; they age out with the TTL. That is the trade the TTL exists
//! to bound, and the reason its default is an hour rather than a day.
//!
//! ## Scoping
//!
//! Caching is opt-in **per API token** ([`crate::api_keys::ApiKeyPermission::cache`]).
//! The policy also picks the blast radius of a cached answer — see
//! [`CacheScope`]. Every scope is contained within one Chalk environment: the
//! environment id is always part of the digest, so entries can never cross the
//! tenancy boundary even when one Valkey instance is shared.
//!
//! ## Backends
//!
//! [`PromptCache`] is the pluggable seam. Two implementations ship:
//! [`memory::MemoryPromptCache`] (bounded, LRU, in-process) and
//! [`valkey::ValkeyPromptCache`] (shared across replicas). A cache is never
//! load-bearing: every backend swallows its own failures and reports a miss, so
//! an unreachable Valkey degrades to uncached serving rather than failing
//! requests.

pub mod memory;
pub mod valkey;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use http::HeaderMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{PromptCacheBackendKind, PromptCacheConfig};
use crate::hashing::hex_encode;
use crate::types::ChatCompletionRequest;

/// Bumped whenever the digest inputs or the stored value's encoding change, so
/// a deployment rolling forward can never serve an entry computed under the old
/// rules (rather than needing the cache flushed by hand).
const CACHE_FORMAT_VERSION: &str = "chalk-router/prompt-cache/v3";

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

/// Which requests may share one cached answer.
///
/// Ordered from widest to narrowest. Every scope is already partitioned by
/// *model*, because the requested model id is part of the request digest — so
/// no scope can ever serve one model's answer for another model's request.
/// What the scope chooses is the **tenancy** partition on top of that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheScope {
    /// Shared by every token in the environment, partitioned only by model.
    ///
    /// The widest and highest-hit-rate scope, and the one to think hardest
    /// about: one token's completion is served to another token's identical
    /// request. It also makes cache *membership* observable — a token can learn
    /// that some other token asked a particular question, by asking it and
    /// seeing a hit. (Response timing would leak the same thing, so this is
    /// inherent to sharing a cache, not something the header adds.)
    /// Appropriate when every token in the environment belongs to the same
    /// trust domain (a shared eval harness, a single product surface), not when
    /// tokens are handed to mutually-untrusted callers.
    Model,
    /// Partitioned by the key's usage pool — Chalk's unit of team ownership.
    ///
    /// A key with no usage pool has no team to share within, so it degrades to
    /// [`CacheScope::Token`] rather than pooling every pool-less key in the
    /// environment together (which would be a silent widening, not a narrowing).
    Team,
    /// Partitioned by the API token itself. Nothing is shared between keys.
    ///
    /// The default, and the only scope with no cross-token disclosure at all.
    #[default]
    Token,
}

impl CacheScope {
    pub fn as_str(self) -> &'static str {
        match self {
            CacheScope::Model => "model",
            CacheScope::Team => "team",
            CacheScope::Token => "token",
        }
    }
}

/// Per-token caching policy, stored on the key's
/// [`crate::api_keys::ApiKeyPermission`].
///
/// Absent (`None` on the permission) means the token is not cached at all —
/// caching is opt-in per token, never inherited from a deployment default.
/// `ttl_secs` / `max_entry_bytes` left unset inherit the deployment's
/// [`PromptCacheConfig`], so an operator can retune every opted-in token at
/// once without rewriting keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachePolicy {
    /// Kill switch for this token, so caching can be turned off without losing
    /// the configured scope/TTL.
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "is_default_scope")]
    pub scope: CacheScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entry_bytes: Option<u64>,
}

fn is_default_scope(scope: &CacheScope) -> bool {
    *scope == CacheScope::default()
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            scope: CacheScope::default(),
            ttl_secs: None,
            max_entry_bytes: None,
        }
    }
}

impl CachePolicy {
    /// An enabled policy at the given scope, inheriting the deployment's TTL
    /// and entry-size ceiling.
    pub fn new(scope: CacheScope) -> Self {
        Self {
            scope,
            ..Self::default()
        }
    }

    fn ttl(&self, defaults: &PromptCacheConfig) -> Duration {
        match self.ttl_secs {
            Some(secs) if secs > 0 => Duration::from_secs(secs),
            // An explicit `0` is a request for "no TTL of my own", which is the
            // same inheritance the absent case gets — a zero-length TTL would
            // make every entry stale on arrival, which no caller means.
            _ => defaults.ttl,
        }
    }

    /// The entry-size ceiling, clamped to the deployment's: a token may make
    /// its own limit tighter but never wider, so one key can't push
    /// multi-megabyte responses into a shared Valkey.
    fn max_entry_bytes(&self, defaults: &PromptCacheConfig) -> usize {
        match self.max_entry_bytes {
            Some(bytes) if bytes > 0 => (bytes as usize).min(defaults.max_entry_bytes),
            _ => defaults.max_entry_bytes,
        }
    }
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// A hash-addressed cache key: the SHA-256 of the framed digest inputs.
///
/// The raw prompt is never stored, in the key or anywhere else — an entry is
/// addressed only by its digest, so a cache dump (or a shared Valkey an
/// operator can inspect) exposes no conversation text through key names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CacheKey([u8; 32]);

impl CacheKey {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Hex rendering, used as the Valkey key suffix and as the span attribute
    /// that lets a hit be correlated with the miss that populated it.
    pub fn to_hex(self) -> String {
        hex_encode(&self.0)
    }
}

/// Accumulates length-framed fields into a digest.
///
/// Every field is prefixed with its byte length, so no combination of field
/// values can produce the same byte stream as a different combination (plain
/// concatenation would let `("ab", "c")` and `("a", "bc")` collide, which for
/// a tenancy-partitioned key is a cross-tenant hit).
struct KeyDigest(Sha256);

impl KeyDigest {
    fn new() -> Self {
        let mut this = Self(Sha256::new());
        this.field(CACHE_FORMAT_VERSION.as_bytes());
        this
    }

    fn field(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.update((bytes.len() as u64).to_be_bytes());
        self.0.update(bytes);
        self
    }

    fn finish(self) -> CacheKey {
        CacheKey(self.0.finalize().into())
    }
}

/// Who is asking — the inputs [`CacheScope`] partitions on.
#[derive(Debug, Clone, Copy)]
pub struct CacheIdentity<'a> {
    /// The Chalk environment this deployment serves. Always part of the
    /// digest, at every scope: a shared Valkey must never join two tenants.
    pub environment_id: &'a str,
    /// The key's usage pool, if any — the `team` scope's partition.
    pub usage_pool_id: Option<&'a str>,
    /// The requesting key's `key_hash` (never the raw secret).
    pub key_hash: &'a str,
}

/// The API surface a digest was computed for. Keeps the two request shapes in
/// disjoint key spaces, so an Anthropic Messages body can never collide with
/// an OpenAI chat body that happens to hash the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// `/v1/chat/completions`, digested from the parsed request.
    OpenAiChat,
    /// `/v1/messages`, digested from the raw request body.
    AnthropicMessages,
}

impl Surface {
    fn as_str(self) -> &'static str {
        match self {
            Surface::OpenAiChat => "openai.chat",
            Surface::AnthropicMessages => "anthropic.messages",
        }
    }
}

/// Resolve the scope's tenancy partition, degrading `team` to `token` for a key
/// that has no usage pool (see [`CacheScope::Team`]).
fn effective_scope(scope: CacheScope, identity: &CacheIdentity<'_>) -> CacheScope {
    match scope {
        CacheScope::Team if identity.usage_pool_id.is_none() => CacheScope::Token,
        other => other,
    }
}

fn write_scope(digest: &mut KeyDigest, scope: CacheScope, identity: &CacheIdentity<'_>) {
    digest.field(scope.as_str().as_bytes());
    match scope {
        CacheScope::Model => {}
        CacheScope::Team => {
            digest.field(identity.usage_pool_id.unwrap_or_default().as_bytes());
        }
        CacheScope::Token => {
            digest.field(identity.key_hash.as_bytes());
        }
    };
}

/// Canonical JSON for the parts of a chat request that determine the answer.
///
/// Ordinary JSON values retain their existing sorted-map behavior. Schema
/// property maps preserve declaration order; harmless formatting, keyword order
/// and numeric spelling are canonicalized using the existing Value semantics.
/// Duplicate-bearing schemas are uncacheable because provider interpretation
/// may differ.
///
/// Excluded: `stream` and `stream_options` (a streamed and a non-streamed ask
/// for the same prompt share one entry, since the stored value is the same
/// canonical response either way) and `user` (an opaque end-user tag OpenAI
/// defines as abuse-tracking metadata, which does not change the completion —
/// leaving it in would shard the cache per end user under the wider scopes and
/// defeat them).
///
/// Note this digests the *parsed* request, so a field
/// [`ChatCompletionRequest`] does not model (`seed`, `logprobs`, …) is not part
/// of the key. That is not a gap: serde drops such fields at deserialization,
/// so the router never forwards them upstream either — the cache is exactly as
/// faithful as the request the router actually makes. Adding a field to
/// [`ChatCompletionRequest`] automatically adds it to the digest; a field the
/// router started forwarding *without* modelling would not, and would need
/// `CACHE_FORMAT_VERSION` bumped.
fn canonical_chat_request(req: &ChatCompletionRequest) -> Option<Vec<u8>> {
    use serde_json::value::{to_raw_value, RawValue};
    let mut fields: std::collections::BTreeMap<&'static str, Box<RawValue>> =
        std::collections::BTreeMap::new();
    // `stream`, `stream_options`, `user` and the unserialized `fallbacks` are
    // deliberately absent. Every other field is emitted exactly as the
    // whole-struct serialization would: absent when `None`.
    fields.insert("model", to_raw_value(&req.model).ok()?);
    fields.insert("messages", to_raw_value(&req.messages).ok()?);
    macro_rules! optional {
        ($($name:ident),*) => {$(
            if let Some(value) = &req.$name {
                fields.insert(stringify!($name), to_raw_value(value).ok()?);
            }
        )*};
    }
    optional!(
        temperature,
        top_p,
        n,
        stop,
        max_tokens,
        max_completion_tokens,
        presence_penalty,
        frequency_penalty,
        logit_bias,
        tool_choice,
        parallel_tool_calls
    );
    if let Some(format) = &req.response_format {
        let format = format.canonical_response_format_for_cache().ok()?;
        fields.insert("response_format", to_raw_value(&format).ok()?);
    }
    if let Some(tools) = &req.tools {
        // Only the tool definitions are copied; messages never are.
        let mut canonical = Vec::with_capacity(tools.len());
        for tool in tools {
            let mut tool = tool.clone();
            if let Some(schema) = &tool.function.parameters {
                tool.function.parameters = Some(schema.canonical_for_cache().ok()?);
            }
            canonical.push(tool);
        }
        fields.insert("tools", to_raw_value(&canonical).ok()?);
    }
    serde_json::to_vec(&fields).ok()
}

/// The previous clone-and-reparse implementation, kept to prove the new
/// one is byte-identical.
#[cfg(test)]
fn canonical_chat_request_reference(req: &ChatCompletionRequest) -> Option<Vec<u8>> {
    let mut req = req.clone();
    if let Some(format) = &mut req.response_format {
        *format = format.canonical_response_format_for_cache().ok()?;
    }
    if let Some(tools) = &mut req.tools {
        for tool in tools {
            if let Some(schema) = &mut tool.function.parameters {
                *schema = schema.canonical_for_cache().ok()?;
            }
        }
    }
    let encoded = serde_json::to_vec(&req).ok()?;
    let mut fields: std::collections::BTreeMap<String, Box<serde_json::value::RawValue>> =
        serde_json::from_slice(&encoded).ok()?;
    fields.remove("stream");
    fields.remove("stream_options");
    fields.remove("user");
    serde_json::to_vec(&fields).ok()
}

/// Digest an OpenAI-shaped chat request.
///
/// `None` when the request cannot be canonicalized (it does not serialize back
/// to a JSON object) — an uncacheable request, never a request served from a
/// guessed key.
pub fn chat_cache_key(
    scope: CacheScope,
    identity: &CacheIdentity<'_>,
    req: &ChatCompletionRequest,
) -> Option<CacheKey> {
    let canonical = canonical_chat_request(req)?;
    let mut digest = KeyDigest::new();
    digest.field(identity.environment_id.as_bytes());
    write_scope(&mut digest, scope, identity);
    digest.field(Surface::OpenAiChat.as_str().as_bytes());
    digest.field(&canonical);
    Some(digest.finish())
}

/// Native identity retains nested wire ordering and every supported option.
pub fn native_chat_cache_key(
    scope: CacheScope,
    identity: &CacheIdentity<'_>,
    request: &crate::native_chat::NativeChatRequest,
) -> Option<CacheKey> {
    let fields = request.cache_fields()?;
    let bytes = serde_json::to_vec(&fields).ok()?;
    let mut digest = KeyDigest::new();
    digest.field(b"native-chat/v1");
    digest.field(identity.environment_id.as_bytes());
    write_scope(&mut digest, scope, identity);
    digest.field(Surface::OpenAiChat.as_str().as_bytes());
    digest.field(&bytes);
    Some(digest.finish())
}

/// The request headers that change a passthrough response's *shape* without
/// changing a byte of the request body.
///
/// A passthrough entry stores the upstream bytes verbatim, so every input that
/// alters what upstream returns has to be in the key — otherwise a caller is
/// served a recording of a materially different question. The body covers
/// almost all of it; `anthropic-beta` is the hole, because opting into a beta
/// is a header, and several betas change the response shape on their own:
/// `thinking-binding-controls-2026-08-01` adds an `input_transformations`
/// array to every response, `thinking-display-updates-2026-08-18` changes
/// which `thinking` blocks stream back, `cache-diagnosis-2026-04-07` adds
/// `diagnostics`.
///
/// The thinking-binding case is the one that makes this a correctness bug
/// rather than a cosmetic one. `input_transformations` is the only report of
/// which thinking blocks the API dropped from the conversation; replaying an
/// opted-out caller's response to an opted-in one erases that report, and the
/// caller concludes nothing was dropped. Under `CacheScope::Model` the two
/// callers need not even be the same token.
///
/// `anthropic-version` is keyed for the same reason at a coarser grain: it
/// pins the wire format the response is rendered in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponseShapeHeaders {
    /// `anthropic-beta`, normalized: comma-split, trimmed, blanks dropped,
    /// sorted and deduped. The opted-in betas are a *set*, so a caller that
    /// reorders or repeats them is asking for the same bytes and must not
    /// shard the cache.
    betas: Vec<String>,
    /// `anthropic-version`, trimmed but otherwise verbatim — an opaque date
    /// string with no set semantics to normalize.
    version: String,
}

impl ResponseShapeHeaders {
    /// Extract the shaping headers from a client request.
    pub fn from_headers(headers: &HeaderMap) -> Self {
        // A header may legitimately be sent more than once, and the API reads
        // the repeats as one comma-joined list — so flatten across values
        // rather than taking only the first.
        let mut betas: Vec<String> = headers
            .get_all("anthropic-beta")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        betas.sort_unstable();
        betas.dedup();

        let version = headers
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .trim()
            .to_string();

        Self { betas, version }
    }

    fn write(&self, digest: &mut KeyDigest) {
        // Frame the count as well as each name. `KeyDigest::field` already
        // makes a single beta unambiguous, but the count keeps a *list* of
        // them from colliding with a shorter list followed by the version.
        digest.field(&(self.betas.len() as u64).to_be_bytes());
        for beta in &self.betas {
            digest.field(beta.as_bytes());
        }
        digest.field(self.version.as_bytes());
    }
}

/// Digest a raw request body (the Anthropic Messages passthrough).
///
/// The body is hashed as the client sent it, so — unlike the chat surface —
/// `stream` *is* part of the key and a streamed ask does not share an entry
/// with a non-streamed one. That is deliberate: this surface caches the
/// upstream bytes verbatim rather than a canonical response, so the two shapes
/// are genuinely different stored values.
///
/// `shape` covers the request headers that steer the response shape without
/// appearing in the body — see [`ResponseShapeHeaders`].
pub fn raw_body_cache_key(
    scope: CacheScope,
    identity: &CacheIdentity<'_>,
    surface: Surface,
    shape: &ResponseShapeHeaders,
    body: &[u8],
) -> CacheKey {
    let mut digest = KeyDigest::new();
    digest.field(identity.environment_id.as_bytes());
    write_scope(&mut digest, scope, identity);
    digest.field(surface.as_str().as_bytes());
    shape.write(&mut digest);
    digest.field(body);
    digest.finish()
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// How a cached payload must be replayed to the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadKind {
    /// A complete JSON response body. On a streaming request the caller
    /// re-emits it as SSE.
    Json,
    /// A complete SSE byte stream, replayed verbatim.
    Sse,
}

impl PayloadKind {
    fn tag(self) -> u8 {
        match self {
            PayloadKind::Json => b'j',
            PayloadKind::Sse => b's',
        }
    }

    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            b'j' => Some(PayloadKind::Json),
            b's' => Some(PayloadKind::Sse),
            _ => None,
        }
    }
}

/// A stored completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedCompletion {
    pub payload: Vec<u8>,
    pub payload_kind: PayloadKind,
    /// The public model id that actually served the request when the entry was
    /// stored — a judge downgrade or a fallback may have moved it off the
    /// requested model. Surfaced on a hit as `x-chalk-routed-model` so a cached
    /// answer reports the same routing the live answer did.
    pub served_model: String,
    /// Wall-clock store time, for the `age` reported on a hit.
    pub stored_at_unix: u64,
}

impl CachedCompletion {
    pub fn new(payload: Vec<u8>, payload_kind: PayloadKind, served_model: String) -> Self {
        Self {
            payload,
            payload_kind,
            served_model,
            stored_at_unix: now_unix(),
        }
    }

    /// Total bytes this entry occupies — the payload plus its bookkeeping — for
    /// the entry-size ceiling and the in-memory backend's byte budget. Measuring
    /// the whole entry rather than just the payload keeps the memory backend's
    /// budget honest; against the megabyte-scale ceilings this is compared to,
    /// the constant overhead is noise.
    pub fn weight(&self) -> usize {
        self.payload.len() + self.served_model.len() + std::mem::size_of::<Self>()
    }

    pub fn age(&self) -> Duration {
        Duration::from_secs(now_unix().saturating_sub(self.stored_at_unix))
    }

    /// Wire encoding for backends that store opaque bytes.
    ///
    /// A framed header followed by the payload verbatim, rather than JSON: the
    /// payload is already-serialized response bytes, and re-encoding it as a
    /// JSON string (or a base64 blob) would inflate every entry for nothing.
    /// Header fields cannot contain `\n` (a payload kind tag, a model id, and
    /// a decimal timestamp), so a single split is unambiguous.
    pub fn encode(&self) -> Vec<u8> {
        let header = format!(
            "{}\n{}\n{}\n",
            self.payload_kind.tag() as char,
            self.served_model.replace('\n', " "),
            self.stored_at_unix
        );
        let mut out = Vec::with_capacity(header.len() + self.payload.len());
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Decode [`Self::encode`]. `None` for anything malformed — a corrupt or
    /// foreign entry reads as a miss rather than as a response.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut lines = bytes.splitn(4, |&b| b == b'\n');
        let kind = PayloadKind::from_tag(*lines.next()?.first()?)?;
        let served_model = std::str::from_utf8(lines.next()?).ok()?.to_string();
        let stored_at_unix = std::str::from_utf8(lines.next()?).ok()?.parse().ok()?;
        let payload = lines.next()?.to_vec();
        Some(Self {
            payload,
            payload_kind: kind,
            served_model,
            stored_at_unix,
        })
    }
}

pub(crate) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Backend trait
// ---------------------------------------------------------------------------

/// A pluggable store of cached completions.
///
/// Implementations must be **infallible from the caller's perspective**: a
/// backend that is down, slow, or returning garbage reports a miss and logs,
/// and `put` is best-effort. The request path treats the cache as an
/// optimization it can always do without, so no error type is surfaced here to
/// tempt a caller into failing a request over it.
#[async_trait]
pub trait PromptCache: Send + Sync {
    /// Backend name, for logs and the `chalk.router.cache.backend` attribute.
    fn kind(&self) -> &'static str;

    /// Whether this deployment can cache at all. `false` short-circuits the
    /// per-request policy lookup and the key derivation.
    fn enabled(&self) -> bool {
        true
    }

    async fn get(&self, key: &CacheKey) -> Option<CachedCompletion>;

    /// Store an entry, expiring after `ttl`. Overwrites any existing entry for
    /// the key.
    async fn put(&self, key: &CacheKey, value: CachedCompletion, ttl: Duration);
}

/// The default: no caching, no allocation, no per-request work.
pub struct DisabledPromptCache;

#[async_trait]
impl PromptCache for DisabledPromptCache {
    fn kind(&self) -> &'static str {
        "disabled"
    }
    fn enabled(&self) -> bool {
        false
    }
    async fn get(&self, _key: &CacheKey) -> Option<CachedCompletion> {
        None
    }
    async fn put(&self, _key: &CacheKey, _value: CachedCompletion, _ttl: Duration) {}
}

/// Construct the backend selected by configuration.
///
/// Never fails: an unconstructable backend logs and degrades to
/// [`DisabledPromptCache`], because a cache that cannot be built is a reason to
/// serve uncached, not a reason to refuse to boot. (The Valkey backend connects
/// lazily, so a Valkey that is merely down at boot still comes up and starts
/// hitting once it recovers.)
pub fn from_config(config: &PromptCacheConfig) -> Arc<dyn PromptCache> {
    match config.backend {
        PromptCacheBackendKind::Disabled => Arc::new(DisabledPromptCache),
        PromptCacheBackendKind::Memory => {
            tracing::info!(
                max_entries = config.max_entries,
                max_bytes = config.max_bytes,
                ttl_secs = config.ttl.as_secs(),
                "Prompt cache enabled (in-memory LRU; per-replica, lost on restart)"
            );
            Arc::new(memory::MemoryPromptCache::new(
                config.max_entries,
                config.max_bytes,
            ))
        }
        PromptCacheBackendKind::Valkey => match config.url.as_deref() {
            Some(url) => match valkey::ValkeyPromptCache::new(url, &config.namespace) {
                Ok(cache) => {
                    tracing::info!(
                        namespace = %config.namespace,
                        ttl_secs = config.ttl.as_secs(),
                        "Prompt cache enabled (valkey)"
                    );
                    Arc::new(cache)
                }
                Err(e) => {
                    tracing::error!(error = %e, "Invalid ROUTER_PROMPT_CACHE_URL; prompt cache disabled");
                    Arc::new(DisabledPromptCache)
                }
            },
            None => {
                tracing::error!(
                    "ROUTER_PROMPT_CACHE_BACKEND=valkey requires ROUTER_PROMPT_CACHE_URL; prompt cache disabled"
                );
                Arc::new(DisabledPromptCache)
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Request-path helper
// ---------------------------------------------------------------------------

/// Everything the request path needs to consult and populate the cache for one
/// request. Built once per request by [`context_for`]; `None` means this
/// request is not cacheable and the path pays nothing further.
#[derive(Debug, Clone)]
pub struct CacheContext {
    pub key: CacheKey,
    /// The scope after the `team`-without-a-pool degradation, i.e. what
    /// actually partitioned the key. Reported on the span so an operator sees
    /// the effective scope rather than the configured one.
    pub scope: CacheScope,
    pub ttl: Duration,
    pub max_entry_bytes: usize,
}

impl CacheContext {
    /// Whether an entry of this size may be stored. Oversized responses are
    /// dropped rather than truncated — half a completion is not a completion.
    pub fn admits(&self, value: &CachedCompletion) -> bool {
        value.weight() <= self.max_entry_bytes
    }
}

/// Build the cache context for a request, or `None` when the deployment has no
/// cache backend, the token has not opted in, or the request could not be
/// canonicalized.
///
/// `digest` supplies the surface-specific part of the key, so both request
/// shapes share the policy resolution, the scope degradation, and the ceilings.
pub fn context_for(
    cache: &dyn PromptCache,
    policy: Option<&CachePolicy>,
    config: &PromptCacheConfig,
    identity: &CacheIdentity<'_>,
    digest: impl FnOnce(CacheScope) -> Option<CacheKey>,
) -> Option<CacheContext> {
    if !cache.enabled() {
        return None;
    }
    let policy = policy.filter(|p| p.enabled)?;
    let scope = effective_scope(policy.scope, identity);
    Some(CacheContext {
        key: digest(scope)?,
        scope,
        ttl: policy.ttl(config),
        max_entry_bytes: policy.max_entry_bytes(config),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChatMessage;

    fn identity<'a>(pool: Option<&'a str>, key_hash: &'a str) -> CacheIdentity<'a> {
        CacheIdentity {
            environment_id: "env-1",
            usage_pool_id: pool,
            key_hash,
        }
    }

    fn req(model: &str, prompt: &str) -> ChatCompletionRequest {
        ChatCompletionRequest {
            fallbacks: Default::default(),
            model: model.to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: Some(serde_json::Value::String(prompt.to_string())),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            temperature: None,
            top_p: None,
            n: None,
            stream: None,
            stop: None,
            max_tokens: None,
            max_completion_tokens: None,
            presence_penalty: None,
            frequency_penalty: None,
            logit_bias: None,
            user: None,
            tools: None,
            tool_choice: None,
            response_format: None,
            stream_options: None,
            parallel_tool_calls: None,
            reasoning_effort: None,
        }
    }

    #[test]
    fn canonical_chat_request_matches_the_clone_and_reparse_reference() {
        let schema = r#"{"type":"object","properties":{"zeta":{"type":"number","minimum":1.2300e-4},"alpha":{"type":"string"}},"required":["zeta"]}"#;
        let cases = [
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#.to_string(),
            r#"{"model":"m","messages":[
                {"role":"system","content":[{"type":"text","text":"a","meta":{"z":1.2300e-4,"a":[1,2.50,1e10]}}]},
                {"role":"user","content":{"k":{"y":1,"x":2.0},"n":-0.0}},
                {"role":"assistant","content":null,"tool_calls":[{"id":"c","type":"function","function":{"name":"f","arguments":"{}"}}]},
                {"role":"tool","content":"r","tool_call_id":"c","name":"f"}]}"#
                .to_string(),
            format!(
                r#"{{"model":"m","messages":[{{"role":"user","content":"x"}}],"tools":[
                    {{"type":"function","function":{{"name":"a","description":"d","parameters":{schema}}}}},
                    {{"type":"function","function":{{"name":"b"}}}}]}}"#
            ),
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"response_format":{"type":"json_schema","json_schema":{"name":"n","strict":true,"schema":{"type":"object","properties":{"zz":{"type":"string"},"aa":{"type":"integer"}}}}}}"#.to_string(),
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"stream":true,"stream_options":{"include_usage":true},"user":"u1"}"#.to_string(),
            format!(
                r#"{{"model":"m","messages":[{{"role":"user","content":"x"}}],"temperature":0.7,"top_p":0.9,"n":2,"stop":["a","b"],"max_tokens":10,"max_completion_tokens":20,"presence_penalty":0.1,"frequency_penalty":-0.2,"logit_bias":{{"50256":-100,"1":1.50}},"tool_choice":{{"type":"function","function":{{"name":"a"}}}},"parallel_tool_calls":false,"tools":[{{"type":"function","function":{{"name":"a","parameters":{schema}}}}}],"response_format":{{"type":"json_object"}}}}"#
            ),
        ];
        for case in cases {
            let req: ChatCompletionRequest = serde_json::from_str(&case).unwrap();
            let new = canonical_chat_request(&req).expect("cacheable");
            let old = canonical_chat_request_reference(&req).expect("cacheable");
            assert_eq!(
                String::from_utf8_lossy(&new),
                String::from_utf8_lossy(&old),
                "{case}"
            );
        }
        let duplicate: ChatCompletionRequest = serde_json::from_str(
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"a","parameters":{"properties":{"z":{},"z":{}}}}}]}"#,
        )
        .unwrap();
        assert!(canonical_chat_request(&duplicate).is_none());
        assert!(canonical_chat_request_reference(&duplicate).is_none());
    }

    fn key(scope: CacheScope, id: &CacheIdentity<'_>, r: &ChatCompletionRequest) -> CacheKey {
        chat_cache_key(scope, id, r).expect("chat request canonicalizes")
    }

    #[test]
    fn identical_requests_share_a_key() {
        let id = identity(None, "k1");
        assert_eq!(
            key(CacheScope::Token, &id, &req("chalk/smart", "hi")),
            key(CacheScope::Token, &id, &req("chalk/smart", "hi"))
        );
    }

    #[test]
    fn a_changed_prefix_changes_the_key() {
        let id = identity(None, "k1");
        assert_ne!(
            key(CacheScope::Token, &id, &req("chalk/smart", "hi")),
            key(CacheScope::Token, &id, &req("chalk/smart", "hi ")),
        );
        // One more turn of the same conversation is a different prefix.
        let mut longer = req("chalk/smart", "hi");
        longer.messages.push(ChatMessage {
            role: "assistant".to_string(),
            content: Some(serde_json::Value::String("hello".to_string())),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        });
        assert_ne!(
            key(CacheScope::Token, &id, &req("chalk/smart", "hi")),
            key(CacheScope::Token, &id, &longer)
        );
    }

    #[test]
    fn generation_parameters_are_part_of_the_key() {
        let id = identity(None, "k1");
        let base = req("chalk/smart", "hi");
        let mut hotter = base.clone();
        hotter.temperature = Some(0.9);
        let mut capped = base.clone();
        capped.max_tokens = Some(16);
        assert_ne!(
            key(CacheScope::Token, &id, &base),
            key(CacheScope::Token, &id, &hotter)
        );
        assert_ne!(
            key(CacheScope::Token, &id, &base),
            key(CacheScope::Token, &id, &capped)
        );
    }

    #[test]
    fn schema_cache_keys_ignore_formatting_but_preserve_value_and_property_semantics() {
        let id = identity(None, "k1");
        let make = |schema: &str, tools: bool| {
            let field = if tools {
                format!(
                    r#""tools":[{{"type":"function","function":{{"name":"lookup","parameters":{schema}}}}}]"#
                )
            } else {
                format!(
                    r#""response_format":{{"type":"json_schema","json_schema":{{"schema":{schema}}}}}"#
                )
            };
            serde_json::from_str::<ChatCompletionRequest>(&format!(
                r#"{{"model":"chalk/smart","messages":[],{field}}}"#
            ))
            .unwrap()
        };
        for tools in [false, true] {
            let first = make(
                r#"{"type":"object","properties":{"z":{"minimum":1.0,"items":{"properties":{"q":{},"b":{}}}},"a":{"title":"hello"}}}"#,
                tools,
            );
            let equivalent = make(
                r#"{ "properties": {"z": {"items": {"properties": {"q": {}, "b": {}}}, "minimum": 1.00}, "a": {"title":"h\u0065llo"}}, "type": "object" }"#,
                tools,
            );
            let exponent = make(
                r#"{"type":"object","properties":{"z":{"minimum":1e0,"items":{"properties":{"q":{},"b":{}}}},"a":{"title":"hello"}}}"#,
                tools,
            );
            assert_eq!(
                key(CacheScope::Token, &id, &first),
                key(CacheScope::Token, &id, &equivalent)
            );
            assert_eq!(
                key(CacheScope::Token, &id, &first),
                key(CacheScope::Token, &id, &exponent)
            );
            let integer = make(
                r#"{"type":"object","properties":{"z":{"minimum":1,"items":{"properties":{"q":{},"b":{}}}},"a":{"title":"hello"}}}"#,
                tools,
            );
            assert_ne!(
                key(CacheScope::Token, &id, &first),
                key(CacheScope::Token, &id, &integer)
            );
            let nested_reordered = make(
                r#"{"type":"object","properties":{"z":{"minimum":1.0,"items":{"properties":{"b":{},"q":{}}}},"a":{"title":"hello"}}}"#,
                tools,
            );
            assert_ne!(
                key(CacheScope::Token, &id, &first),
                key(CacheScope::Token, &id, &nested_reordered)
            );
            let duplicate = make(r#"{"properties":{"z":{},"z":{}}}"#, tools);
            assert!(chat_cache_key(CacheScope::Token, &id, &duplicate).is_none());
        }
    }

    #[test]
    fn schema_cache_keys_follow_schema_context_instead_of_literal_property_names() {
        let id = identity(None, "k1");
        let make = |schema: &str, tools: bool| {
            let field = if tools {
                format!(
                    r#""tools":[{{"type":"function","function":{{"name":"lookup","parameters":{schema}}}}}]"#
                )
            } else {
                format!(
                    r#""response_format":{{"type":"json_schema","json_schema":{{"schema":{schema}}}}}"#
                )
            };
            serde_json::from_str::<ChatCompletionRequest>(&format!(
                r#"{{"model":"chalk/smart","messages":[],{field}}}"#
            ))
            .unwrap()
        };
        let first = r#"{"properties":{"properties":{"type":"object","description":"x","properties":{"z":{},"a":{}}}},"enum":[{"properties":{"z":1,"a":2}}],"default":{"properties":{"z":1,"a":2}},"const":{"properties":{"z":1,"a":2}}}"#;
        let equivalent = r#"{"const":{"properties":{"a":2,"z":1}},"default":{"properties":{"a":2,"z":1}},"enum":[{"properties":{"a":2,"z":1}}],"properties":{"properties":{"description":"x","properties":{"z":{},"a":{}},"type":"object"}}}"#;
        for tools in [false, true] {
            assert_eq!(
                key(CacheScope::Token, &id, &make(first, tools)),
                key(CacheScope::Token, &id, &make(equivalent, tools))
            );
            let reordered = equivalent.replace(r#""z":{},"a":{}"#, r#""a":{},"z":{}"#);
            assert_ne!(
                key(CacheScope::Token, &id, &make(first, tools)),
                key(CacheScope::Token, &id, &make(&reordered, tools))
            );
        }
    }

    #[test]
    fn schema_order_partitions_cache_but_top_level_field_order_does_not() {
        let id = identity(None, "k1");
        let make = |properties: &str, tools: bool| {
            let schema = format!(r#"{{"properties":{properties}}}"#);
            let options = if tools {
                format!(
                    r#""tools":[{{"type":"function","function":{{"name":"lookup","parameters":{schema}}}}}]"#
                )
            } else {
                format!(
                    r#""response_format":{{"type":"json_schema","json_schema":{{"schema":{schema}}}}}"#
                )
            };
            serde_json::from_str::<ChatCompletionRequest>(&format!(r#"{{"model":"chalk/smart","messages":[{{"role":"user","content":"hi"}}],{options}}}"#)).unwrap()
        };
        for tools in [false, true] {
            let first = make(r#"{"z":{},"a":{}}"#, tools);
            let reordered = make(r#"{"a":{},"z":{}}"#, tools);
            assert_ne!(
                key(CacheScope::Token, &id, &first),
                key(CacheScope::Token, &id, &reordered)
            );
        }
        let first: ChatCompletionRequest = serde_json::from_str(
            r#"{"model":"chalk/smart","messages":[{"role":"user","content":{"z":1,"a":2}}]}"#,
        )
        .unwrap();
        let reordered: ChatCompletionRequest = serde_json::from_str(
            r#"{"messages":[{"content":{"a":2,"z":1},"role":"user"}],"model":"chalk/smart"}"#,
        )
        .unwrap();
        assert_eq!(
            key(CacheScope::Token, &id, &first),
            key(CacheScope::Token, &id, &reordered)
        );
    }

    #[test]
    fn the_model_partitions_every_scope() {
        let id = identity(Some("pool"), "k1");
        for scope in [CacheScope::Model, CacheScope::Team, CacheScope::Token] {
            assert_ne!(
                key(scope, &id, &req("chalk/smart", "hi")),
                key(scope, &id, &req("chalk/cheap", "hi")),
                "{scope:?} must not share an entry across models"
            );
        }
    }

    #[test]
    fn streaming_shares_an_entry_with_non_streaming() {
        let id = identity(None, "k1");
        let mut streamed = req("chalk/smart", "hi");
        streamed.stream = Some(true);
        assert_eq!(
            key(CacheScope::Token, &id, &req("chalk/smart", "hi")),
            key(CacheScope::Token, &id, &streamed)
        );
    }

    #[test]
    fn the_end_user_tag_does_not_shard_the_cache() {
        let id = identity(None, "k1");
        let mut tagged = req("chalk/smart", "hi");
        tagged.user = Some("end-user-42".to_string());
        assert_eq!(
            key(CacheScope::Token, &id, &req("chalk/smart", "hi")),
            key(CacheScope::Token, &id, &tagged)
        );
    }

    #[test]
    fn token_scope_isolates_keys_but_model_scope_shares() {
        let a = identity(Some("pool"), "key-a");
        let b = identity(Some("pool"), "key-b");
        let r = req("chalk/smart", "hi");
        assert_ne!(
            key(CacheScope::Token, &a, &r),
            key(CacheScope::Token, &b, &r)
        );
        assert_eq!(
            key(CacheScope::Model, &a, &r),
            key(CacheScope::Model, &b, &r)
        );
    }

    #[test]
    fn team_scope_shares_within_a_pool_and_isolates_across_pools() {
        let r = req("chalk/smart", "hi");
        let a = identity(Some("pool-1"), "key-a");
        let b = identity(Some("pool-1"), "key-b");
        let other = identity(Some("pool-2"), "key-c");
        assert_eq!(key(CacheScope::Team, &a, &r), key(CacheScope::Team, &b, &r));
        assert_ne!(
            key(CacheScope::Team, &a, &r),
            key(CacheScope::Team, &other, &r)
        );
    }

    #[test]
    fn team_scope_without_a_pool_degrades_to_token_scope() {
        let a = identity(None, "key-a");
        let b = identity(None, "key-b");
        assert_eq!(effective_scope(CacheScope::Team, &a), CacheScope::Token);
        // ...and the degraded keys are the token-scoped ones, so two pool-less
        // keys never share an entry.
        let r = req("chalk/smart", "hi");
        assert_eq!(
            key(effective_scope(CacheScope::Team, &a), &a, &r),
            key(CacheScope::Token, &a, &r)
        );
        assert_ne!(
            key(effective_scope(CacheScope::Team, &a), &a, &r),
            key(effective_scope(CacheScope::Team, &b), &b, &r)
        );
    }

    #[test]
    fn the_environment_partitions_every_scope() {
        let r = req("chalk/smart", "hi");
        let here = CacheIdentity {
            environment_id: "env-1",
            usage_pool_id: None,
            key_hash: "k",
        };
        let there = CacheIdentity {
            environment_id: "env-2",
            usage_pool_id: None,
            key_hash: "k",
        };
        for scope in [CacheScope::Model, CacheScope::Team, CacheScope::Token] {
            assert_ne!(key(scope, &here, &r), key(scope, &there, &r));
        }
    }

    #[test]
    fn scope_fields_cannot_collide_across_boundaries() {
        // Length framing: an environment/pool split one character over must not
        // digest the same as the original.
        let r = req("chalk/smart", "hi");
        let a = CacheIdentity {
            environment_id: "envx",
            usage_pool_id: Some("1"),
            key_hash: "k",
        };
        let b = CacheIdentity {
            environment_id: "env",
            usage_pool_id: Some("x1"),
            key_hash: "k",
        };
        assert_ne!(key(CacheScope::Team, &a, &r), key(CacheScope::Team, &b, &r));
    }

    /// Build a `HeaderMap` from `(name, value)` pairs, appending repeats.
    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                http::header::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                value.parse().expect("header value"),
            );
        }
        map
    }

    fn shape(pairs: &[(&str, &str)]) -> ResponseShapeHeaders {
        ResponseShapeHeaders::from_headers(&headers(pairs))
    }

    #[test]
    fn surfaces_have_disjoint_key_spaces() {
        let id = identity(None, "k1");
        let body = br#"{"model":"claude","messages":[]}"#;
        let none = ResponseShapeHeaders::default();
        let raw = raw_body_cache_key(
            CacheScope::Token,
            &id,
            Surface::AnthropicMessages,
            &none,
            body,
        );
        let chat = raw_body_cache_key(CacheScope::Token, &id, Surface::OpenAiChat, &none, body);
        assert_ne!(raw, chat);
    }

    #[test]
    fn raw_bodies_digest_byte_for_byte() {
        let id = identity(None, "k1");
        let none = ResponseShapeHeaders::default();
        let k = |b: &[u8]| {
            raw_body_cache_key(CacheScope::Token, &id, Surface::AnthropicMessages, &none, b)
        };
        assert_eq!(k(b"{\"a\":1}"), k(b"{\"a\":1}"));
        assert_ne!(k(b"{\"a\":1}"), k(b"{\"a\":2}"));
        // Unlike the chat surface, `stream` is part of the body and so the key.
        assert_ne!(k(br#"{"stream":true}"#), k(br#"{"stream":false}"#));
    }

    #[test]
    fn opting_into_a_beta_does_not_hit_the_opted_out_entry() {
        // The bug this keys against: `anthropic-beta` changes the response
        // shape without changing a body byte, so an opted-in caller was served
        // the opted-out recording. For thinking-binding controls that means
        // losing `input_transformations` — the only report of which thinking
        // blocks the API dropped.
        let id = identity(None, "k1");
        let body = br#"{"model":"claude-opus-5","messages":[]}"#;
        let k = |s: &ResponseShapeHeaders| {
            raw_body_cache_key(CacheScope::Token, &id, Surface::AnthropicMessages, s, body)
        };

        let plain = shape(&[]);
        let binding = shape(&[("anthropic-beta", "thinking-binding-controls-2026-08-01")]);
        assert_ne!(k(&plain), k(&binding));

        // ...and two distinct betas are distinct entries, not just present/absent.
        let display = shape(&[("anthropic-beta", "thinking-display-updates-2026-08-18")]);
        assert_ne!(k(&binding), k(&display));
    }

    #[test]
    fn beta_lists_are_a_set_not_a_string() {
        // Reordering, whitespace, repeats, and splitting the list across
        // repeated headers all describe the same opt-in — they must not shard
        // the cache, or a caller changing SDKs loses every hit.
        let one = "compact-2026-01-12,thinking-binding-controls-2026-08-01";
        let canonical = shape(&[("anthropic-beta", one)]);

        for equivalent in [
            "thinking-binding-controls-2026-08-01,compact-2026-01-12",
            " compact-2026-01-12 ,  thinking-binding-controls-2026-08-01 ",
            "compact-2026-01-12,thinking-binding-controls-2026-08-01,compact-2026-01-12",
            "compact-2026-01-12,,thinking-binding-controls-2026-08-01",
        ] {
            assert_eq!(
                canonical,
                shape(&[("anthropic-beta", equivalent)]),
                "{equivalent}"
            );
        }

        // Repeated headers are read as one comma-joined list by the API.
        assert_eq!(
            canonical,
            shape(&[
                ("anthropic-beta", "thinking-binding-controls-2026-08-01"),
                ("anthropic-beta", "compact-2026-01-12"),
            ])
        );

        // An absent header and an empty one both mean "no betas".
        assert_eq!(shape(&[]), shape(&[("anthropic-beta", "")]));
    }

    #[test]
    fn a_beta_list_cannot_forge_a_different_one() {
        // Length framing plus the framed count: no regrouping of the same
        // characters across the beta set and the version can collide.
        let id = identity(None, "k1");
        let body = b"{}";
        let k = |s: &ResponseShapeHeaders| {
            raw_body_cache_key(CacheScope::Token, &id, Surface::AnthropicMessages, s, body)
        };
        let split = shape(&[
            ("anthropic-beta", "ab,c"),
            ("anthropic-version", "2023-06-01"),
        ]);
        let joined = shape(&[
            ("anthropic-beta", "a,bc"),
            ("anthropic-version", "2023-06-01"),
        ]);
        assert_ne!(k(&split), k(&joined));

        // The version is keyed too, and cannot be absorbed into the beta set.
        let versioned = shape(&[("anthropic-beta", "a"), ("anthropic-version", "2023-06-01")]);
        let unversioned = shape(&[("anthropic-beta", "a")]);
        assert_ne!(k(&versioned), k(&unversioned));
    }

    #[test]
    fn value_encoding_round_trips() {
        let value = CachedCompletion::new(
            b"{\"id\":\"x\"}\n\nweird\nbytes".to_vec(),
            PayloadKind::Json,
            "chalk/smart".to_string(),
        );
        let decoded = CachedCompletion::decode(&value.encode()).expect("decodes");
        assert_eq!(decoded, value);
    }

    #[test]
    fn sse_payloads_round_trip_verbatim() {
        let payload = b"data: {\"a\":1}\n\ndata: [DONE]\n\n".to_vec();
        let value =
            CachedCompletion::new(payload.clone(), PayloadKind::Sse, "chalk/smart".to_string());
        let decoded = CachedCompletion::decode(&value.encode()).expect("decodes");
        assert_eq!(decoded.payload, payload);
        assert_eq!(decoded.payload_kind, PayloadKind::Sse);
    }

    #[test]
    fn malformed_entries_decode_to_none() {
        assert!(CachedCompletion::decode(b"").is_none());
        assert!(CachedCompletion::decode(b"j\n").is_none());
        // Unknown payload-kind tag (a value written by a future version).
        assert!(CachedCompletion::decode(b"z\nmodel\n1\npayload").is_none());
        // Non-numeric timestamp.
        assert!(CachedCompletion::decode(b"j\nmodel\nnope\npayload").is_none());
    }

    #[test]
    fn policy_ttl_and_size_inherit_and_clamp() {
        let mut config = PromptCacheConfig::disabled();
        config.ttl = Duration::from_secs(600);
        config.max_entry_bytes = 1_000;

        let inherit = CachePolicy::new(CacheScope::Token);
        assert_eq!(inherit.ttl(&config), Duration::from_secs(600));
        assert_eq!(inherit.max_entry_bytes(&config), 1_000);

        let tighter = CachePolicy {
            ttl_secs: Some(30),
            max_entry_bytes: Some(100),
            ..CachePolicy::new(CacheScope::Token)
        };
        assert_eq!(tighter.ttl(&config), Duration::from_secs(30));
        assert_eq!(tighter.max_entry_bytes(&config), 100);

        // A token may not raise the deployment's entry ceiling.
        let greedy = CachePolicy {
            max_entry_bytes: Some(10_000_000),
            ..CachePolicy::new(CacheScope::Token)
        };
        assert_eq!(greedy.max_entry_bytes(&config), 1_000);

        // Zero means "inherit", not "expire instantly" / "store nothing".
        let zeroed = CachePolicy {
            ttl_secs: Some(0),
            max_entry_bytes: Some(0),
            ..CachePolicy::new(CacheScope::Token)
        };
        assert_eq!(zeroed.ttl(&config), Duration::from_secs(600));
        assert_eq!(zeroed.max_entry_bytes(&config), 1_000);
    }

    #[test]
    fn context_is_none_without_a_backend_or_an_opted_in_token() {
        let config = PromptCacheConfig::disabled();
        let id = identity(None, "k1");
        let r = req("chalk/smart", "hi");
        let enabled = memory::MemoryPromptCache::new(4, 4096);
        let policy = CachePolicy::new(CacheScope::Token);

        // No backend.
        assert!(
            context_for(&DisabledPromptCache, Some(&policy), &config, &id, |s| {
                chat_cache_key(s, &id, &r)
            })
            .is_none()
        );
        // Backend, but the token never opted in.
        assert!(
            context_for(&enabled, None, &config, &id, |s| chat_cache_key(s, &id, &r)).is_none()
        );
        // Backend, but the token's policy is switched off.
        let off = CachePolicy {
            enabled: false,
            ..policy.clone()
        };
        assert!(
            context_for(&enabled, Some(&off), &config, &id, |s| chat_cache_key(
                s, &id, &r
            ))
            .is_none()
        );
        // Both present.
        assert!(
            context_for(&enabled, Some(&policy), &config, &id, |s| chat_cache_key(
                s, &id, &r
            ))
            .is_some()
        );
    }

    #[test]
    fn context_reports_the_degraded_scope() {
        let config = PromptCacheConfig::disabled();
        let id = identity(None, "k1");
        let r = req("chalk/smart", "hi");
        let cache = memory::MemoryPromptCache::new(4, 4096);
        let policy = CachePolicy::new(CacheScope::Team);
        let ctx = context_for(&cache, Some(&policy), &config, &id, |s| {
            chat_cache_key(s, &id, &r)
        })
        .expect("context");
        assert_eq!(ctx.scope, CacheScope::Token);
    }

    #[test]
    fn oversized_entries_are_not_admitted() {
        let ctx = CacheContext {
            key: CacheKey([0; 32]),
            scope: CacheScope::Token,
            ttl: Duration::from_secs(60),
            max_entry_bytes: 256,
        };
        let small = CachedCompletion::new(vec![0; 1], PayloadKind::Json, "m".to_string());
        let big = CachedCompletion::new(vec![0; 4096], PayloadKind::Json, "m".to_string());
        assert!(ctx.admits(&small));
        assert!(!ctx.admits(&big));
    }
}
