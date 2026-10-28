# RFP-0004: Per-Model Capability Profiles and Protocol Adapters

- **Status:** Accepted / Implemented
- **Created:** 2026-07-23
- **Revised:** 2026-08-27 — completed Bedrock Runtime streaming and capability-based Playground filtering
- **Depends on:** [RFP-0001](0001-router-motivation-and-architecture.md) — provider connections, model routes, resolve-once contract
- **Revision:** Combines the original per-model modality proposal with the expanded per-operation protocol capability design
- **Related:** [RFP-0002](0002-streaming-fallback.md) — fallback eligibility; [RFP-0003](0003-permission-scope-non-llm-endpoints.md) — capabilities do not grant permission
- **Superseded in part by:** [RFP-0005](0005-effective-per-model-capabilities.md) — effective per-model facts, schema semantics, public/admin projections, and Playground filtering
- **Scope:** Define one capability model spanning model surfaces, modalities, features, and endpoint operations; expose a stable public projection; use it to enable native Responses, broaden Anthropic ingress, add an internal Bedrock Converse adapter, and canonicalize Grok/xAI as `xai`.

---

## Implementation state

Implemented on `main`:

- the versioned capability vocabulary, provider defaults, sparse connection
  declarations, resolution precedence/provenance, and conflict reporting;
- public `/v1/models` capability profiles, provider admin editing, and the VS
  Code projection/export;
- native/translated Responses create and streaming, operation-specific gates,
  opaque response locators, continuation, compact, retrieve, and delete;
- capability-selected Anthropic native, adapted, and translated ingress;
- Bedrock Mantle plus the Bedrock Runtime Converse, ConverseStream, and
  CountTokens adapters;
- canonical `xai` provider identity, with the former `grok` provider kind and
  prefix removed as a breaking cutover; and
- explicit-`Unsupported` pre-dispatch enforcement, permissive `Unknown`, and
  capability-aware fallback filtering; and
- capability-based Chat, embedding, and image model selection in the Playground.

The phase descriptions below are retained as the design record and annotated
with their current implementation state.

## 1. Introduction

The router now needs to answer several questions that sound similar but are not
the same:

1. What can the **model** do — generate text, accept images, call tools, embed,
   or rerank?
2. Which **upstream endpoint** can carry that request — OpenAI Responses,
   Chat Completions, Anthropic Messages, Bedrock Converse, or a translation?
3. Which operations and features does that particular endpoint preserve —
   streaming, stored-response retrieval, compaction, background execution, or
   server-side tools?
4. What may we safely promise to a **client** such as the Playground or VS Code?
5. Where did the answer come from, and did the connection explicitly declare it?

The original version of this RFP correctly made model capability a set,
separated input modality from dispatchable surface, and treated native Responses
as a separate protocol axis. Its flat `capabilities` and `native_apis` sketches
were not expressive enough: an endpoint may be native but partial, one operation
may be adapted while another is unsupported, and a model route may accept images
through one ingress but not another. This revision retains those principles and
replaces the original storage and wire sketches with the unified design below.

This RFP introduces a versioned **resolved capability profile**. It is the common
source for runtime dispatch, `/v1/models`, admin diagnostics, Playground filters,
and client-specific projections. It keeps public promises separate from internal
routing mechanics.

## 2. Problem

### 2.1 Pre-implementation baseline

Before this RFP was implemented:

- `Model` is only `{id, object, created, owned_by}` (`src/types.rs`).
- Connections and model routes have no capability metadata.
- `OpenAICompatibleBackend` has one `native_responses: bool`; only the direct OpenAI
  constructor sets it. That cannot describe partial Groq support, stored
  Responses operations, or server tools.
- `/v1/messages` resolution hard-requires `provider_kind == "anthropic"`, even
  when the resolved provider exposes an official Anthropic Messages endpoint.
- `BuiltinProvider.adapter` chooses one broad backend constructor. It does not
  describe the several protocol surfaces a connection may expose.
- The Playground infers model type from names. Downstream config generators have
  no canonical source for booleans such as `toolCalling` and `vision`.
- `grok` and `xai` are distinct provider kinds even though both target xAI's API.

### 2.2 A boolean or flat set fails

`native_responses = true` does not answer whether `retrieve`, `delete`,
`compact`, background mode, `previous_response_id`, or a particular server tool
works. Likewise, `native_apis: ["responses"]` incorrectly suggests full OpenAI
parity.

The same problem appears on Anthropic ingress. A provider may support native
Messages but not count-tokens, or it may implement count-tokens through a
provider-specific equivalent. Treating the pair as one boolean either hides a
useful route or over-promises another.

### 2.3 Model truth and endpoint truth are different

A vision-capable model may accept image input through Messages but not through a
provider's OpenAI adapter. A model may support tool calling while a translated
ingress loses strict schemas or parallel calls. Model-level features are
necessary but insufficient; clients need the intersection of model and selected
endpoint capabilities.

### 2.4 Aliases and fallback need conservative capabilities

A public alias may move between deployments or use fallback. Its advertised
capabilities cannot be the union of every internal candidate: that would promise
a feature that only some destinations can serve. In the MVP, an alias inherits
its primary target's resolved profile, and internal fallback may choose only
candidates satisfying the request's requirements. A separately declared alias
contract is deferred until a concrete use case requires one.

### 2.5 Unknown is not false

Most OpenAI-compatible `/v1/models` responses do not carry capability metadata.
A failed probe also does not prove lack of support. The design therefore needs a
real `unknown` state; absent knowledge must never be silently converted to
`unsupported` in the canonical profile.

### 2.6 Constraints inherited from the other RFPs

The other RFPs do not define a model capability schema, but they constrain this
one:

- **RFP-0001:** provider registry, connection, public route, and resolved request
  identity are separate objects. Capabilities must join the same resolution
  snapshot and obey the resolve-once contract.
- **RFP-0002:** fallback attempts are resolved independently; an ineligible
  capability match is a skipped candidate, not a dispatch failure, and must not
  overwrite the last upstream error.
- **RFP-0003:** advisory metadata and enforcement are separate rollout choices.
  Capability support also remains orthogonal to API-key permission.

## 3. Ecosystem findings

### 3.1 LiteLLM

LiteLLM maintains a rich internal per-model catalog. Its schema includes a
primary `mode`, `supported_endpoints`, supported input/output modalities,
`supported_openai_params`, and granular flags such as vision, function calling,
reasoning, prompt caching, and native streaming. Its call layer separately maps
public routes to operation types such as Responses, Anthropic Messages, rerank,
OCR, and realtime. See LiteLLM's
[`ModelInfo` types](https://github.com/BerriAI/litellm/blob/main/litellm/types/utils.py)
and [model catalog schema](https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.schema.json).

The proxy keeps the ordinary OpenAI model list separate from its richer
`/v1/model/info` management/discovery response. Configured public model names are
aliases over underlying `litellm_params.model`, while `model_info` holds
metadata. The useful lesson is the separation; the part not to copy is a large
collection of overlapping booleans without a uniform provenance or endpoint
resolution rule.

### 3.2 Fireworks and Nexus

Fireworks exposes small OpenAI-style model objects on the inference surface,
while its account management model objects contain deployment and training
metadata such as state, model type, context length, and serverless eligibility.
Capabilities are primarily documented by inference endpoint: Chat, Responses,
Embeddings, Rerank, and Anthropic-compatible Messages. See the
[inference model object](https://docs.fireworks.ai/tools-sdks/python-client/api-reference),
[management model API](https://docs.fireworks.ai/api-reference/get-model), and
[Responses API](https://docs.fireworks.ai/guides/response-api).

Nexus presents a stable model/router choice to coding harnesses and internally
selects an execution model. Fireworks routers likewise use a stable router name
in the request's `model` field while deployments can change behind it. See
[Fireworks Nexus](https://fireworks.ai/nexus) and
[Fireworks Routers](https://docs.fireworks.ai/deployments/routers).

The public documentation does not expose a general Nexus capability schema, so
we should not infer one. The observable pattern is nevertheless useful: keep the
public model/alias contract small and stable; keep deployment membership,
provider mechanics, and mutable evidence in the management plane.

### 3.3 Conclusions for Chalk

- Keep `/v1/models` OpenAI-compatible and additive; do not expose credentials,
  base URLs, fallback membership, or mutable probe internals.
- Provide a richer admin view with the optional connection declaration,
  resolved values, provenance, and conflicts.
- Model endpoint operations explicitly instead of inferring them from provider
  names or one `mode` field.
- For an alias, advertise the primary target's profile and ensure fallback does
  not enlarge it; a future smart route uses a conservative guaranteed
  intersection, never a union of candidate features.

## 4. Goals and non-goals

### Goals

- One versioned vocabulary for model surfaces, input/output modalities, model
  features, endpoint operations, and endpoint-specific constraints.
- One optional sparse declaration at provider-connection scope, resolved over
  ephemeral model discovery and code-owned provider defaults with per-field
  provenance.
- A conservative public effective profile and richer admin profile.
- Deterministic projections for Playground and clients such as VS Code.
- Runtime request requirement extraction and capability-aware fallback.
- Native Responses profiles, broader Anthropic ingress, Bedrock Converse as an
  internal adapter, and one canonical xAI provider.

### Non-goals

- Maintaining a hand-curated, exhaustive catalog for every vendor model.
- Treating transient connection-test results as routing truth.
- Guaranteeing arbitrary, unknown fields merely because an endpoint is native.
- Making capabilities an authorization grant; API-key permission remains a
  separate check.
- Exposing a public `/converse` endpoint in the first implementation.
- Perfect capability detection from model names.

### MVP scope

- Ship conservative, code-owned defaults for every built-in provider and its
  known model families. These defaults require no database configuration.
- Accept one optional sparse `declared_capabilities` document on a provider
  connection for endpoint-specific facts that defaults and discovery cannot
  know.
- Keep discovered model metadata ephemeral and use it between the connection
  declaration and built-in defaults during resolution.
- Persist no route-level or per-model capability records, create no reusable
  capability-profile resource, and accept no request headers that alter
  capability truth.
- Expose resolved capabilities publicly and declaration provenance only through
  the admin plane. Enforce explicit `Unsupported` only after an advisory phase.

## 5. Terminology and semantics

### 5.1 Support state

```rust
pub enum Support {
    Supported,
    Unsupported,
    Unknown,
}
```

`Unknown` is first-class. In a sparse connection declaration, an absent key means
"inherit from the next source"; an explicit `Unknown` masks a lower-confidence
default when an operator knows it cannot be trusted.

### 5.2 Delivery mode

```rust
pub enum DeliveryMode {
    /// Same public protocol and semantics are sent to an equivalent upstream endpoint.
    Native,
    /// A provider-specific semantic adapter is used, e.g. Bedrock Converse or Vertex rawPredict.
    Adapted,
    /// A generic cross-protocol compatibility codec is used, e.g. Responses -> Chat.
    Translated,
    Unsupported,
    Unknown,
}
```

`Adapted` and `Translated` are deliberately distinct. An adapted implementation
can understand and validate a provider-specific semantic equivalent. A translated
implementation uses a general compatibility bridge and is normally a narrower,
potentially lossy subset. Neither may silently drop unsupported request fields.

### 5.3 Capability axes

The canonical profile has five independent axes:

1. **Surfaces** — router workloads: `chat`, `embeddings`, `image_generation`,
   `rerank`, `moderation`, `audio_transcription`, `audio_speech`, `realtime`.
2. **Input modalities** — `text`, `image`, `audio`, `video`, `pdf`.
3. **Output modalities** — `text`, `image`, `audio`, `video`, `embeddings`.
4. **Model features** — initially `tool_calling`, `parallel_tool_calling`,
   `strict_tool_schema`, `structured_output`, `reasoning`, `prompt_caching`,
   `computer_use`; new closed keys are additive.
5. **Protocols and operations** — how a particular ingress operation reaches
   the upstream and which features that path preserves.

Limits such as `context_window`, `max_input_tokens`, and `max_output_tokens` are
optional scalar metadata in the same document. They are useful to clients but
do not replace upstream validation.

## 6. Canonical schema

### 6.1 Resolved JSON shape

```yaml
schema_version: 1
surfaces:
  chat: supported
  embeddings: unsupported
modalities:
  input:
    text: supported
    image: supported
    audio: unknown
  output:
    text: supported
features:
  tool_calling: supported
  parallel_tool_calling: supported
  strict_tool_schema: unknown
  structured_output: supported
limits:
  context_window: 200000
  max_output_tokens: 64000
protocols:
  openai_responses:
    operations:
      create:
        mode: native
        features:
          streaming: supported
          stored_continuation: supported
          background: unsupported
          tool_calling: supported
          image_input: supported
          server_tool.web_search: unsupported
          server_tool.file_search: unsupported
          server_tool.code_interpreter: unsupported
          server_tool.computer_use: unsupported
          server_tool.mcp: unknown
      retrieve: { mode: native }
      delete: { mode: native }
      compact: { mode: unsupported }
      cancel: { mode: unsupported }
      list_input_items: { mode: unknown }
  openai_chat_completions:
    operations:
      create:
        mode: native
        features:
          streaming: supported
          tool_calling: supported
          image_input: supported
  anthropic_messages:
    operations:
      create:
        mode: native
        features:
          streaming: supported
          tool_calling: supported
          image_input: supported
      count_tokens: { mode: adapted }
```

The protocol profile is **per operation**, not one mode for the whole protocol.
This directly represents the motivating examples:

```text
ResponsesCapabilities.create       -> protocols.openai_responses.operations.create.mode
ResponsesCapabilities.stream       -> create.features.streaming
ResponsesCapabilities.retrieve     -> operations.retrieve.mode
ResponsesCapabilities.delete       -> operations.delete.mode
ResponsesCapabilities.compact      -> operations.compact.mode
ResponsesCapabilities.background   -> create.features.background
ResponsesCapabilities.server_tools -> create.features.server_tool.*

anthropic_messages                  -> anthropic_messages.operations.create.mode
anthropic_count_tokens              -> anthropic_messages.operations.count_tokens.mode
```

### 6.2 Why protocol features can repeat model features

The model-level `tool_calling: supported` means the model can call tools. The
Responses operation's `tool_calling: unsupported` means that endpoint or adapter
cannot preserve it. The client-facing answer for that path is their conservative
intersection:

```text
Supported   + Supported   = Supported
Unsupported + anything    = Unsupported
anything    + Unsupported = Unsupported
Unknown     + Supported   = Unknown
Supported   + Unknown     = Unknown
```

This is how a vision-capable model can correctly project `vision: false` for an
endpoint that cannot carry image input.

### 6.3 Rust shape

Use typed enums internally and serialize them to stable snake-case strings:

```rust
pub struct CapabilityProfile {
    pub schema_version: u32,
    pub surfaces: BTreeMap<ModelSurface, Support>,
    pub input_modalities: BTreeMap<InputModality, Support>,
    pub output_modalities: BTreeMap<OutputModality, Support>,
    pub features: BTreeMap<ModelFeature, Support>,
    pub limits: ModelLimits,
    pub protocols: BTreeMap<Protocol, ProtocolProfile>,
}

pub struct ProtocolProfile {
    pub operations: BTreeMap<ProtocolOperation, OperationProfile>,
}

pub struct OperationProfile {
    pub mode: DeliveryMode,
    pub features: BTreeMap<EndpointFeature, Support>,
}
```

`CapabilityPatch` has the same shape but is sparse. Resolution is a field-wise
overlay, not replacement of an entire nested object.

## 7. Sources, storage, and resolution

### 7.1 Sources and precedence

Resolve every leaf independently in this order:

1. `provider_connections.declared_capabilities` — optional sparse declaration
   for this concrete endpoint/server configuration.
2. Capability metadata discovered for that model on that connection.
3. Built-in registry/adapter defaults, including conservative defaults for
   `vllm` and `openai-compatible`.
4. `Unknown`.

A connection declaration outranks live discovery because it describes the
operator's concrete server configuration. Discovery may prefill the connection
form and raise drift diagnostics, but it does not silently rewrite the stored
declaration.

### 7.2 Persistence

The original draft's `TEXT[]` columns cannot carry protocol operations. The MVP
adds one nullable, versioned JSONB declaration to provider connections:

```sql
ALTER TABLE chalk_router.provider_connections
    ADD COLUMN declared_capabilities JSONB;
```

JSON is validated into typed Rust before persistence. The document carries
`schema_version`; unknown future versions fail writes and are surfaced as
unreadable configuration rather than silently ignored.

`NULL` means "use discovery and built-in defaults" and is the normal state for
built-in providers. There is no backfill of operator data. The field is written
only when explicitly present in connection create/update input or a seed file;
model listing, discovery, probes, and requests never persist it.

#### Why only connection-level declarations

A declaration describes the concrete endpoint rather than a reusable model
resource. It is useful when server configuration affects capability:

- A custom OpenAI-compatible or vLLM server may be generation, pooling, rerank,
  or transcription oriented even though `/v1/models` does not say which.
- Native Responses, automatic tool calling, reasoning parsers, structured
  output, and multimodal support may depend on server version and launch flags.
- A cloud endpoint's region or adapter configuration may intentionally support
  less than the provider-wide default.

The declaration is small, unique to its connection, and updated with that
connection. Giving it an independent id would add a CRUD lifecycle, join,
orphan policy, and shared-mutation behavior without a reuse case.

The MVP deliberately has no route-level declaration and no per-model override
table. A `chalk/*` route inherits the profile resolved for its upstream model and
connection. This is enough for the current one-model-per-vLLM/common custom
deployment shape and avoids speculative persistence for mixed catalogs.

If operators later need centrally managed capability templates applied to many
routes or different declarations for models sharing one connection, add those
resources only after that use case appears.

#### Built-in and custom defaults

Every built-in provider has a code-owned conservative profile. In particular,
the `vllm` and `openai-compatible` defaults are text chat, text input/output,
streaming, and translated Responses through Chat. Native Responses, tool
calling, vision, structured output, embeddings, rerank, and audio remain
`Unknown` unless discovery proves them or the connection explicitly declares
them. Client projections turn those unknown advanced features into `false`.

### 7.3 Discovered evidence

Discovery is stored in the existing per-connection model discovery cache, keyed
by connection identity/fingerprint and model id. It is not persisted. A catalog
field supplied by an upstream is evidence; a bounded successful feature probe
may add `Supported`; a failed request usually remains `Unknown`, because failure
may be auth, quota, model policy, or transient health.

Such probes are explicit admin/UI detection actions, not request-path or
background routing inputs. Their results may prefill declarations or remain
ephemeral evidence; they never silently change the operator contract.

The initial **Detect** action probes only cheap chat generation (`max_tokens: 1`)
and embeddings with a short input. It never probes image generation, audio, or
other slow/billable modalities. New custom OpenAI-compatible/vLLM forms may
suggest `chat`, the common case, but the submitted declaration remains the only
durable value.

On-demand connection-test results remain completely separate. They answer "did
this probe work now?"; capabilities answer "what contract is declared or
evidenced?"

### 7.4 Resolved route contract

`ResolvedRoute` gains `capabilities: Arc<ResolvedCapabilityProfile>`. The same
snapshot read must resolve backend, stripped model, attribution, and capabilities
once. This preserves RFP-0001's resolve-once/TOCTOU guarantee.

A connection declaration change invalidates the resolution snapshot but does not
rebuild an HTTP backend: it is not part of the existing credential/base URL
fingerprint. Typed connection options that change transport construction, such
as Bedrock Mantle versus Runtime, must be added to that fingerprint.

Each resolved leaf records provenance for the admin plane:

```text
connection_declaration | upstream_discovery | provider_registry | unknown
```

Public responses omit per-field provenance; admin RPCs expose it and any
declaration/discovery conflict.

### 7.5 Alias and fallback guarantees

- A `chalk/*` route advertises its primary target's resolved profile. Every
  fallback candidate must satisfy the actual request requirements before it is
  eligible; fallback does not enlarge the advertised profile.
- Route-level public contracts are deferred. A future smart route that may
  choose a destination before dispatch advertises the conservative intersection
  of all eligible candidates unless a separate contract feature is designed.
- Never advertise the union of candidate capabilities.

## 8. Public and admin API design

### 8.1 Public model objects

Extend `Model` additively:

```json
{
  "id": "anthropic/claude-sonnet-5",
  "object": "model",
  "created": 0,
  "owned_by": "anthropic",
  "capabilities": {
    "schema_version": 1,
    "surfaces": {"chat": "supported"},
    "modalities": {
      "input": {"text": "supported", "image": "supported"},
      "output": {"text": "supported"}
    },
    "features": {"tool_calling": "supported"},
    "protocols": {
      "anthropic_messages": {
        "operations": {"create": {"mode": "native"}}
      }
    }
  }
}
```

The field is omitted when the entire profile is unknown during the compatibility
rollout. Unknown individual leaves remain explicit once a profile is emitted.
OpenAI clients ignore the additive field.

`GET /v1/models/:id` returns the same public effective profile. Do not put
connection ids, base URLs, provider credentials, probe errors, or fallback
membership in it.

### 8.2 Admin API

Add an optional typed `declared_capabilities` message to provider-connection
create, update, and read APIs. Admin model and route reads may expose the
resulting profile, but do not accept capability declarations in the MVP. The
admin read shape contains:

```yaml
declared: <sparse connection patch, when applicable>
effective: <resolved profile>
sources: <per-leaf provenance>
conflicts: <declaration vs discovery/registry diagnostics>
```

Use the existing service layer for identical HTTP and gRPC semantics. Updates
follow field-mask semantics so operators can distinguish inherit, explicit
unknown, supported, and unsupported.

The normal UI shows resolved capabilities and provenance first. Editing the
connection declaration is an advanced action for custom endpoints or correcting
discovery. Built-in defaults should not force an operator to copy a large
profile merely to save an unrelated connection field.

### 8.3 VS Code projection

Client-specific config is **derived**, never stored as the source of truth. For
the example Responses URL:

```text
toolCalling = model.features.tool_calling
              INTERSECT responses.create.features.tool_calling
              == Supported

vision      = model.input_modalities.image
              INTERSECT responses.create.features.image_input
              == Supported
```

`Unknown` projects conservatively to `false`; the canonical public/admin profile
still retains `unknown`. The URL comes from the selected public ingress and
deployment configuration, not the capability document. Request headers remain
deployment/client configuration.

Implement this as a reusable `ClientCapabilityProjection` service, initially
used by the UI's VS Code config exporter. It can later project other clients
without adding client-named fields to the canonical model schema.

## 9. Runtime requirements and enforcement

### 9.1 Derive requirements from the request

Handlers derive a `CapabilityRequirement` before dispatch:

```rust
pub struct CapabilityRequirement {
    pub surface: ModelSurface,
    pub protocol: Protocol,
    pub operation: ProtocolOperation,
    pub input_modalities: BTreeSet<InputModality>,
    pub features: BTreeSet<RequiredFeature>,
}
```

Examples:

- A Responses request with an image and function tool requires Responses create,
  image input, and tool calling.
- `previous_response_id` requires stored continuation.
- `background: true` requires background support.
- an Anthropic count request requires Messages `count_tokens`, not Messages
  `create`.

### 9.2 Enforcement state

The advisory rollout is complete. Current behavior is:

- explicit `Unsupported` is rejected cleanly before dispatch;
- `Unknown` and absent leaves remain permissive and defer to upstream
  validation;
- translated/adapted paths reject fields they cannot represent rather than
  silently dropping them; and
- incompatible fallback candidates are skipped without replacing the last real
  dispatch error.

This behavior is code-owned and has no deployment flag. Strict rejection of
`Unknown` is not implemented and would be a separate breaking policy decision.

### 9.3 No request-level capability overrides

Do not accept headers that claim a model supports a capability or replace the
resolved profile. Capability metadata is used for discovery, generated client
configuration, validation, and fallback selection before or across requests; a
per-request assertion cannot provide that stable contract. It would also let a
caller bypass an explicit operator restriction and would make the same model id
advertise different truths depending on an invisible header.

The request already declares its **requirements** through its endpoint and body:
image content requires image input, tools require tool calling,
`previous_response_id` requires continuation, and so on. Runtime compares those
requirements with the resolved profile.

If a future debugging need arises, a header may choose validation policy (for
example, whether `Unknown` is attempted) for authorized callers. Such a header
must never change `Supported`/`Unsupported`, alter `/v1/models`, or force native
versus translated dispatch. No capability-policy header is part of this RFP.

### 9.4 Fallback

Before attempting an alternate, compare it with the request's requirements.
Explicitly incompatible candidates are skipped without replacing `last_err`,
matching RFP-0002's skipped-candidate semantics. A translated Responses
alternate must additionally pass the existing lossless request conversion; a
static profile alone cannot prove an arbitrary evolving request is representable.

## 10. Native Responses capability profiles

### 10.1 Replace the boolean

Remove `OpenAICompatibleBackend.native_responses`. A backend exposes transport primitives;
the resolved capability profile selects native, adapted, or translated dispatch.
`supports_responses(request)` becomes a request requirement/profile check plus
the lossless translator check for `Translated` mode.

The initial registry defaults should be based on official provider endpoints,
not provider-name heuristics:

| Provider | Initial Responses create profile |
|---|---|
| OpenAI | Native |
| Bedrock Mantle | Native, model/feature constrained |
| OpenRouter | Native |
| xAI | Native |
| Qwen | Native |
| Fireworks | Native |
| Groq | Native but explicitly partial/beta |
| vLLM | Unknown by default; declare or discover by version/endpoint |
| Anthropic, Gemini, Vertex, Cohere, Together, Mistral | Translated only when the request is losslessly representable |
| Voyage | Unsupported for text generation |

Native does not imply identical operations. Seed `retrieve`, `delete`,
`compact`, background, stored continuation, and server tools separately from
official documentation and adapter tests.

### 10.2 Complete stateful Responses routing

Implement the public operations the profiles describe in this order:

1. Existing `POST /v1/responses` create/stream.
2. Existing `POST /v1/responses/compact`, gated independently.
3. `GET /v1/responses/:id` and `DELETE /v1/responses/:id`.
4. `GET /v1/responses/:id/input_items` and cancel only when at least one
   supported provider and a client need them.

Retrieve/delete requests carry no model. Chalk must return an opaque router
response id that contains a versioned, integrity-protected locator for
`connection_id + upstream_response_id`. Decode it for `previous_response_id`,
retrieve, delete, and related operations. This preserves resolve-once routing
without a new mutable database lookup and prevents callers from forging a
different connection. Legacy raw upstream ids remain valid only where the
request also has enough model context to resolve safely.

## 11. Anthropic ingress

### 11.1 Resolve by capability, not provider kind

Replace the hard `provider_kind == "anthropic"` check in `resolve_anthropic`
with operation capability checks:

```text
POST /v1/messages
  -> protocols.anthropic_messages.operations.create.mode

POST /v1/messages/count_tokens
  -> protocols.anthropic_messages.operations.count_tokens.mode
```

The initial official profiles are:

| Provider | Messages create | Count tokens |
|---|---|---|
| Anthropic | Native | Native |
| Bedrock Mantle Claude | Native | Native |
| OpenRouter | Native | Unknown/unsupported until documented |
| Fireworks | Native | Unknown/unsupported until documented |
| Qwen | Native | Unknown/unsupported until documented |
| Vertex Claude | Adapted through partner-model API | Adapted |
| vLLM | Unknown by default; version/connection declaration or discovery |
| Other generative providers | Translated only for the documented safe subset |

### 11.2 Adapter contract

Split the backend trait's generic `send_anthropic_request` into operation-aware
native/adapted/translated implementations. Preserve raw pass-through for native
endpoints. Adapted implementations receive parsed typed requests so they can map
provider semantics and reject unsupported blocks. Translated implementations use
one documented Messages-to-Chat codec and emit correct Anthropic streaming event
ordering.

Document the translated subset. At minimum test system messages, mixed content,
image blocks, tool use/results, stop reasons, usage, and streaming block indexes.
Unknown Anthropic fields must not be dropped silently.

## 12. Bedrock adapter

### 12.1 Product surface

Bedrock Converse is an **upstream adapter** behind Chalk's existing Chat,
Responses, and Messages ingresses. Streaming Chat requests use ConverseStream
and translate AWS event-stream messages into incremental OpenAI SSE chunks.
There is no public `/converse` endpoint. A public AWS-compatible path is a
separate product decision because it expands auth, path, and error compatibility
scope.

### 12.2 Backend design

Make Bedrock a real `AdapterKind::Bedrock`, not a thin OpenAI-compatible
constructor. A Bedrock connection may expose:

- Mantle OpenAI Chat and Responses.
- Mantle Anthropic Messages and count-tokens for Claude.
- Bedrock Runtime Converse and ConverseStream as adapted generation transports.
- Runtime CountTokens where the selected model/request supports it.
- InvokeModel is not implemented; add it only as an explicit future escape hatch
  for a concrete modality or feature that Converse cannot represent.

The handler chooses the best supported operation from the resolved profile:
native ingress first, then a semantic adapter, then a validated translation.

### 12.3 Configuration and auth

The implemented MVP uses the existing connection fields rather than adding a
typed Bedrock options document:

- a normal or absent `base_url` selects Mantle and uses the encrypted API key;
- a regional `https://bedrock-runtime.<region>.amazonaws.com` base URL selects
  Runtime, derives the region from the host, and uses the AWS default credential
  chain with SigV4; Runtime credentials require `bedrock:InvokeModel` and
  `bedrock:InvokeModelWithResponseStream`; and
- capability declarations describe supported operations, not credentials.

Do not store a decrypted AWS credential bundle in capability metadata. If the
base-URL convention becomes ambiguous or explicit AWS keys are required, add a
versioned typed connection option and credential variant then.

### 12.4 Bedrock model variability

Converse and Mantle feature support varies by model and region. Connection
declarations describe endpoint availability; code-owned model registry entries
and officially discovered metadata describe model eligibility, modalities,
tools, and features. The resolved result intersects endpoint and model truth, so
declaring an operation native on a connection cannot make an incompatible model
support it. There is no Bedrock per-model declaration in the MVP.

## 13. Canonicalize Grok and xAI

### 13.1 Canonical identity

- Provider kind: `xai`
- Display name: `xAI (Grok)`
- Canonical prefix: `xai/`
- Legacy `grok` provider kind and `grok/` prefix: removed
- Grok remains a model family, not a provider kind.

This matches the external convention used by OpenRouter (`x-ai/grok-*`) and
LiteLLM (`xai/grok-*`). It also prevents duplicate credentials, usage
attribution, permissions, and capability defaults for the same upstream.

### 13.2 Implemented cutover

The implementation chose a direct breaking cutover rather than a compatibility
migration:

1. `grok` is no longer a built-in provider kind and new connections using it are
   rejected.
2. `grok/` is not a prefix alias; callers and seed configuration must use
   `xai/`.
3. Existing deployments must replace any legacy Grok connection configuration
   and provider-scoped permissions explicitly. No automatic row or historical
   usage rewrite is performed.
4. New routing and attribution use `xai` exclusively.

This behavior was shipped with a breaking changelog entry. Reintroducing a
legacy alias would now be a separate compatibility decision, not unfinished
work in this RFP.

## 14. Control-plane and UI state

- The provider form accepts an advanced sparse capability declaration as JSON
  and shows inherited effective capabilities, per-leaf provenance, and declared
  conflicts.
- Public model objects and the Models UI expose effective capabilities. There is
  no route- or per-model declaration resource in the MVP.
- The Playground chooses Responses versus Chat and filters embedding and image
  models from the resolved capability profile. Name heuristics are retained only
  for legacy model objects that omit capabilities; there is no audio Playground
  surface.
- `CapabilityConflict` is exposed on provider connections. Separate
  `CapabilityUnknown` and `LegacyXaiConflict` diagnostics were not added; unknown
  remains visible in the profile and legacy Grok compatibility was removed.
- Seed files accept sparse connection `declared_capabilities`. Existing
  insert-only semantics remain: divergence warns and requires an explicit
  update.

## 15. Observability and diagnostics

Do not add `chalk.capability.*` span attributes. Existing route, provider,
model, HTTP status, and error attributes are sufficient for request tracing.

The connection declaration, provenance, and conflicts belong in the admin
diagnostic response described in section 8.2. A pre-dispatch rejection returns a
specific client error naming the unsupported or unknown requirement and may be
logged normally; it does not need a new tracing taxonomy. Usage attribution
remains keyed by the canonical resolved provider (`xai`, never new `grok`).

## 16. Testing strategy

### 16.1 Schema and resolver

- Serde/proto round trips for every enum and schema version.
- Field-wise precedence, explicit-unknown masking, and sparse clear semantics.
- Model/protocol intersection truth table.
- Connection declaration, discovery, registry, and unknown provenance.
- Primary-profile alias behavior and smart-route intersection behavior.

### 16.2 Runtime

- Requirement extraction from text, image, tools, `previous_response_id`,
  background, server tools, and count-tokens requests.
- Explicit unsupported rejects before dispatch; unknown preserves compatibility.
- Fallback skips an incompatible alternate without overwriting the last dispatch
  error.
- Translated operations reject every non-representable field rather than
  dropping it.

### 16.3 Provider adapters

- Mock contract tests per initial provider profile and operation.
- Native Responses create/stream and each stored-response operation independently.
- Anthropic native/adapted/translated non-streaming and SSE event conformance.
- Bedrock Mantle auth/paths and Runtime SigV4 client construction; Converse tool
  and image mappings; ConverseStream frame fragmentation, text/tool deltas,
  usage, stop reasons, and modeled exceptions.
- Optional paid live tests remain behind `ROUTER_LIVE_API_TESTS=1`.

### 16.4 Public projections and canonical identity

- `/v1/models` omits internal sources but includes stable effective metadata.
- VS Code `toolCalling` and `vision` are true only when both model and selected
  endpoint support them; unknown projects false.
- `grok` is rejected as a provider kind, `xai` remains canonical, and public
  attribution uses `xai`.

## 17. Rollout state

### Phase 0 — Vocabulary and provider matrix (implemented)

- Land the typed schema, merge semantics, and a checked-in provider protocol
  matrix backed by official documentation.
- Add unit tests before changing routing.
- Update RFP-0001's provider list and capability references.

### Phase 1 — Advisory storage and resolution (implemented)

- Add the nullable `provider_connections.declared_capabilities` JSONB column.
- Thread the optional connection patch through stores, services, proto, seed
  input, and the resolution snapshot. Do not add route or per-model capability
  storage.
- Add `ResolvedRoute.capabilities` and admin provenance/conflict output.
- Leave request behavior unchanged.

### Phase 2 — Public discovery and client projections (implemented)

- Extend `/v1/models` and model detail responses.
- Use resolved metadata for Responses-vs-Chat selection and embedding/image
  filtering in the Playground.
- Add the reusable VS Code projection/export path.
- Add the advanced connection declaration editor and diagnostics flow.

### Phase 3 — Responses profiles and operations (implemented)

- Replace `native_responses` with per-operation profiles.
- Enable documented native create/stream for OpenAI, Bedrock Mantle, OpenRouter,
  xAI, Qwen, Fireworks, and restricted Groq; leave vLLM declaration/discovery
  based.
- Add the opaque response locator and implement retrieve/delete.
- Gate compact, background, continuation, and server tools independently.

### Phase 4 — Anthropic ingress (implemented)

- Resolve Messages and count-tokens by operation capability.
- Add native upstream clients for Bedrock Mantle, OpenRouter, Fireworks, and Qwen.
- Add adapted Vertex Claude and Bedrock Runtime paths.
- Enable the documented translated subset for other providers.

### Phase 5 — Bedrock Runtime (implemented)

- `AdapterKind::Bedrock`, Mantle, Runtime Converse, CountTokens, default-chain
  credentials, and SigV4 are implemented.
- Runtime selection currently uses the regional `base_url` convention rather
  than a typed options document.
- ConverseStream translates incremental AWS event-stream text, tool-call, stop,
  usage, and exception events into OpenAI-compatible SSE.
- InvokeModel and a public `/converse` endpoint remain out of scope.

### Phase 6 — xAI canonicalization (implemented as a breaking cutover)

- The `grok` provider kind, adapter, and prefix were removed without a legacy
  alias or automatic data migration.
- New configuration and attribution use `xai`; legacy deployments must update
  configuration and permissions explicitly.

### Phase 7 — Enforcement (implemented)

- Explicit-unsupported gating is active.
- Unknown remains permissive.
- Capability-aware fallback filtering is active.
- Strict unknown rejection remains a separate future policy decision.

## 18. Risks and mitigations

- **Schema sprawl:** use five named axes and per-operation endpoint profiles;
  reject client-specific fields in the canonical schema.
- **Stale declarations:** show inherited/declaration conflicts and provenance;
  never mutate declarations from discovery or explicit tests.
- **False guarantees:** public projections use intersection and unknown is
  conservative; alias routing never advertises a union.
- **Provider drift:** registry profiles are small endpoint defaults, while
  per-model differences stay code-owned/discovered and request validators remain
  authoritative.
- **Translation bugs:** distinguish translated from adapted/native publicly and
  test streaming event semantics; reject unknown non-representable fields.
- **Response object routing:** integrity-protected locators bind follow-up calls
  to the original connection without re-resolution.
- **Breaking xAI cutover:** the changelog calls out removal of `grok`; operators
  must update legacy connections, prefixes, and permissions explicitly.
- **Over-enforcement:** reject only explicit `Unsupported`, keep unknown
  permissive, and do not couple capability checks to connection-test results.

## 19. Decisions

1. Capabilities are a versioned profile, not a flat set or provider boolean.
2. Model truth and endpoint truth are separate and intersected for clients.
3. Protocol delivery mode is per operation and distinguishes native, adapted,
   and translated paths.
4. Unknown is first-class; a missing connection declaration leaf inherits.
5. Public aliases advertise the primary target's profile; fallback candidates do
   not enlarge the public promise. Route contracts are deferred.
6. Public model objects expose effective capabilities; admin APIs expose
   the optional connection declaration, provenance, and conflicts.
7. Bedrock Converse is an internal adapter first, not a new public ingress.
8. `xai` is canonical; the `grok` prefix and provider kind were removed in a
   breaking cutover.

## 20. Primary references for the initial provider matrix

- [AWS Bedrock Mantle OpenAI APIs](https://docs.aws.amazon.com/bedrock/latest/userguide/bedrock-mantle.html)
- [AWS Bedrock Anthropic Messages](https://docs.aws.amazon.com/bedrock/latest/userguide/inference-messages-api.html)
- [AWS Bedrock CountTokens](https://docs.aws.amazon.com/bedrock/latest/userguide/count-tokens.html)
- [OpenRouter Responses](https://openrouter.ai/docs/api/api-reference/responses/create-responses)
- [OpenRouter Anthropic Messages](https://openrouter.ai/docs/api/api-reference/anthropic-messages/create-messages)
- [xAI release notes](https://docs.x.ai/developers/release-notes)
- [Qwen Responses compatibility](https://www.alibabacloud.com/help/en/model-studio/qwen-api-via-openai-responses)
- [Qwen Anthropic Messages compatibility](https://www.alibabacloud.com/help/en/model-studio/anthropic-api-messages)
- [Fireworks Responses](https://docs.fireworks.ai/guides/response-api)
- [Fireworks Anthropic Messages](https://docs.fireworks.ai/api-reference/anthropic-messages)
- [Groq Responses](https://console.groq.com/docs/responses-api)
- [vLLM online serving APIs](https://docs.vllm.ai/en/latest/serving/online_serving/)
- [Vertex AI Claude partner models](https://cloud.google.com/vertex-ai/generative-ai/docs/partner-models/use-claude)
- [Together OpenAI compatibility matrix](https://docs.together.ai/docs/inference/openai-compatibility)
