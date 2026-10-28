# RFP-0005: Effective Per-Model Capabilities and Interface Contracts

- **Status:** Built
- **Created:** 2026-08-27
- **Revised:** 2026-09-03 — documented the later provider-connection model
  catalog RPC without exposing the internal resolver or provenance model
- **Depends on:** [RFP-0001](0001-router-motivation-and-architecture.md) —
  provider connections, model routes, resolve-once contract;
  [RFP-0002](0002-streaming-fallback.md) — fallback eligibility
- **Supersedes:** the capability schema, source-resolution, public projection,
  provider-admin capability, and Playground-filtering portions of
  [RFP-0004](0004-per-model-modality.md). RFP-0004 remains the design record for
  the protocol adapters, Responses operations, Anthropic ingress, Bedrock
  Runtime adapter, and xAI canonicalization already built.
- **Scope:** Replace provider-wide capability promises with an effective contract
  resolved for one model on one connection through one Chalk adapter. Correct
  model discovery, public model metadata, runtime eligibility, protocol
  crossing, provider-admin semantics, and Playground filtering in one pull
  request.

---

## 0. Summary

The current capability vocabulary is expressive, but its facts are too broad.
Most built-in provider functions ignore the model id and attach one generation
profile to every model on the provider. `/v1/models` then publishes that profile
as model truth. This causes both false positives and false negatives: Bedrock
Claude Haiku is advertised as Responses-native even when the upstream rejects
it, while OpenAI and Gemini image models are omitted from the image Playground
because their generic provider profiles advertise only chat.

This RFP keeps capability schema version **1** because no external consumer or
deployed data contract relies on it yet. It changes the version-1 shape before
rollout, rather than preserving semantics known to be wrong.

The effective public contract becomes:

```text
resolved model facts
  INTERSECT resolved connection/endpoint facts
  INTERSECT Chalk adapter facts
  = effective capability of this public model id
```

Hard capability eligibility remains separate from routing quality, cost,
latency, availability, and judge policy. The implementation lands as one pull
request; the independently reviewable changes listed in section 17 land as
separate commits within that PR.

## 1. Database and persistence changes

This section is normative and intentionally precedes the rest of the design.

### 1.1 No DDL change

Do not add a table, column, migration, reusable capability resource, or
route-level capability record. Retain the existing nullable column:

```sql
chalk_router.provider_connections.declared_capabilities JSONB NULL
```

No discovery result, heuristic result, test result, or effective profile is
persisted. `NULL` continues to mean that the connection inherits discovery,
registry, endpoint, and adapter facts.

### 1.2 Stored document shape

The document remains `schema_version: 1`, but its pre-rollout shape changes to
separate model declarations from endpoint declarations:

```json
{
  "schema_version": 1,
  "model_defaults": {
    "surfaces": {},
    "modalities": {"input": {}, "output": {}},
    "features": {},
    "limits": {}
  },
  "models": {
    "optional-canonical-upstream-model-id": {
      "surfaces": {},
      "modalities": {"input": {}, "output": {}},
      "features": {},
      "limits": {}
    }
  },
  "endpoint": {
    "interfaces": {}
  }
}
```

All three sections are optional and sparse:

- `model_defaults` applies to every model on the connection and is useful for a
  single-model custom deployment.
- `models` contains connection-scoped overrides keyed by canonical upstream
  model id. It supports mixed catalogs without creating an independent
  per-model database resource.
- `endpoint` describes only the concrete endpoint or deployment. It cannot by
  itself make an intrinsically incompatible model support a feature.

Model override keys are upstream ids after removal of the connection's Chalk
prefix, not public `chalk/*` route ids. Unknown document keys and unsupported
schema versions fail validation. Create, update, seed, and field-mask semantics
remain unchanged: an absent document inherits, an explicit `null` clears, and a
present sparse document declares only its leaves.

### 1.3 Data migration

There is no production backfill because this contract has not rolled out.
Tests, fixtures, seed examples, and local development rows using the old
version-1 shape must be rewritten or cleared. The implementation must not add a
best-effort parser that silently guesses whether an old document is a model or
endpoint declaration. All declaration structs use strict unknown-field
validation. The PostgreSQL read path must propagate an invalid-document store
error instead of logging the error and treating the declaration as absent. A
startup/release preflight validates every non-null declaration and names the
connection that must be cleared or rewritten without logging credentials.

### 1.4 Why the column remains

`declared_capabilities` is active today, not dead storage. It is overlaid during
every resolved request, model listing, published-route projection, VS Code
projection, and provider-admin response. An explicit unsupported leaf already
changes local runtime gating. The present problem is that one connection-wide
patch is applied to every model and mixes model facts with endpoint facts.
Checked-in seed connections currently leave it null, so deployed row usage is
environment-dependent; this RFP does not assume that any non-null row exists.

Retain the column as the operator-authored correction layer, particularly for
custom OpenAI-compatible endpoints and private deployments whose capabilities
cannot be discovered. Replace its outdated provider-wide document semantics
with the scoped version-1 shape in section 1.2. The raw declaration remains an
advanced admin setting; it is never published as effective model truth.

## 2. RPC and HTTP contract changes

This section is normative and intentionally precedes implementation details.

### 2.1 ProviderConnection RPC changes

Keep `declared_capabilities` on `ProviderConnection`, create, and update
messages. Its `google.protobuf.Struct` contains the schema-version-1 declaration
from section 1.2.

Remove these misleading fields from `ProviderConnection`:

```proto
google.protobuf.Struct effective_capabilities = 12;
map<string, string> capability_sources = 13;
repeated CapabilityConflict capability_conflicts = 14;
```

Delete the fields without reserving numbers 12 through 14. This contract has not
rolled out, so intentional wire incompatibility is acceptable and those numbers
remain available for future connection-scoped fields. A connection can contain
heterogeneous models, so it does not have one effective model profile. Do not
replace these fields with another provider-wide model profile.

Remove the now-unused `CapabilityConflict` protobuf message. Regenerate all Rust
and TypeScript bindings with `just gen-proto`; never edit `ui/src/gen/` manually.

### 2.2 No standalone capability-resolution RPC

Do not add `ModelCapabilityService` or any other public/admin
capability-resolution RPC in this change. OpenRouter and Fireworks expose model
facts through list/detail model resources and separate endpoint/deployment
resources. LiteLLM supplements `/v1/models` with proprietary model-info routes,
but it likewise does not expose a resolver-and-provenance service. Chalk has no
current user workflow that needs another contract.

Implement one wire-agnostic internal resolver in `src/service/` or an equivalent
domain module. Routing, fallback, `/v1/models`, model detail, published routes,
and client projections all call it. Its layered facts, evidence, conflicts, and
delivery modes remain internal typed data available to tests, bounded telemetry,
and future diagnostics. If a concrete admin debugging workflow later requires
provenance, design a read-only API from that workflow rather than preemptively
freezing this internal representation into protobuf.

A later admin workflow added
`ProviderConnectionService.ListProviderConnectionModels`. It lists models
reachable through selected saved connections and returns each model's effective
public capability profile. This is a bounded catalog projection used by model
selectors; it does not expose the resolver, evidence, conflicts, registry rules,
delivery modes, or provenance as a public service. The internal resolution
contract therefore remains private as required here.

### 2.3 Public HTTP model contract

`GET /v1/models` and `GET /v1/models/:id` retain the additive `capabilities`
field, keep `schema_version: 1`, and publish the new effective shape defined in
section 7. These are the user-facing capability discovery surfaces. They do not
publish evidence, registry rules, connection ids, delivery mechanics, discovery
timestamps, or fallback membership.

The list and detail endpoints must return the same effective profile used by
request routing. The original implementation added no capability-filtered
catalog query; the later connection-scoped admin catalog described in §2.2 must
use that same effective profile.

### 2.4 Existing projections

The VS Code projection continues returning its existing client-specific shape,
but derives it from the new effective profile. The Playground uses the normal
model list. Admin HTTP routes that currently serialize connection-level
`effective_capabilities`, `capability_sources`, or `capability_conflicts` remove
those fields without replacement. The connection resource continues exposing
only the raw operator-authored `declared_capabilities` document. Its separate
model-catalog RPC exposes effective per-model public profiles, never one
provider-wide effective profile.

## 3. Problem statement

The current implementation conflates three different facts:

1. a model can perform a workload;
2. a configured endpoint exposes a suitable upstream operation; and
3. Chalk can faithfully serve that workload through a public interface.

The provider registry currently supplies a complete `CapabilityProfile`. Most
provider functions accept a model id but ignore it. Model listing resolves those
provider defaults with no discovered model patch and overwrites any upstream
model metadata. Runtime resolution may apply different connection-specific
patches, so public listing and dispatch can disagree.

Concrete failures include:

- `bedrock/anthropic.claude-haiku-4-5` is advertised as native Responses and is
  forwarded to an upstream endpoint that rejects that model on `/v1/responses`;
- `openai/gpt-image-2` and `gemini/models/gemini-3-pro-image` receive generic
  chat profiles, so a capability-driven image picker hides them;
- an OpenRouter connection advertises native Anthropic Messages for every
  nested model even though OpenRouter documents its Anthropic skin primarily
  for Anthropic models and does not guarantee every model/protocol pairing; and
- the Playground treats a capability-bearing profile as authoritative while
  runtime enforcement remains permissive for missing or unknown leaves.

Provider-wide defaults remain useful evidence about an endpoint or adapter.
They are not effective per-model truth.

## 4. Goals and non-goals

### 4.1 Goals

- Resolve one effective profile for one model on one connection through one
  Chalk adapter.
- Represent model surfaces, modalities, features, limits, public interface
  operations, interface features, and typed parameter constraints.
- Keep `Supported`, `Unsupported`, and `Unknown` distinct.
- Use the same resolution result for public discovery, runtime dispatch,
  fallback eligibility, and client projections.
- Support exact and documented family-level registry facts without scattering
  model-name regular expressions across handlers or UI components.
- Ingest provider-native catalog evidence where it is authoritative.
- Preserve safe OpenAI, Anthropic, and Responses protocol crossing.
- Make capability mismatches observable without treating one upstream failure
  as durable capability truth.

### 4.2 Non-goals

- An exhaustive LiteLLM-sized catalog in the first change.
- A public or admin capability-filtered model-list query.
- Persisted discovery, automatic background probing, or billable probes.
- Route-level capability declarations or an independent per-model CRUD
  resource.
- Inferring quality, coding ability, price, latency, throughput, availability,
  or judge score from capabilities.
- Strict rejection of every `Unknown` capability.
- Mid-stream failover or changes to RFP-0002 retry boundaries.
- Making capabilities an authorization grant.

## 5. Ecosystem findings and design choices

### 5.1 OpenRouter

OpenRouter is the closest operational reference. Its catalog distinguishes
model-level modalities and supported parameters from provider-endpoint records.
Its image catalog defines model-level parameter unions and definitive
per-endpoint subsets with typed descriptors. Its OpenAI Chat Completions surface
is the broad, normalized interface; its Anthropic Messages reference currently
documents Claude models rather than promising that every nested model accepts
Messages. Chalk adopts the model-versus-endpoint separation, per-interface
eligibility, and typed operation constraints.

Its discovery contract is resource-shaped: list models, get one model, and list
the provider endpoints for that model. It does not expose a separate capability
resolution service.

Chalk differs in two ways. A prefixed Chalk model normally resolves to one
configured connection rather than a marketplace of endpoints, and Chalk must
also model the fidelity of its own adapters and translations. Public `chalk/*`
routes therefore advertise a guaranteed contract, never the union of fallback
candidates.

### 5.2 LiteLLM

LiteLLM is the reference for registry breadth: model mode, supported endpoints,
modalities, features, limits, pricing, and deprecation are cataloged per model,
while provider transformations define accepted OpenAI parameters. Chalk adopts
a table-driven per-model registry and provider-specific discovery hooks.

LiteLLM's closest analogues are `/v1/models` plus its proprietary
`/model/info`, `/v2/model/info`, and `/model_group/info` management routes.
Those return configured model/deployment metadata; they are not a per-request
model × endpoint × adapter resolver with provenance.

Chalk does not adopt a growing set of overlapping `supports_*` booleans or mix
pricing with capability resolution. Capability eligibility remains separate
from adjacent catalog metadata and routing policy.

### 5.3 Fireworks and Nexus

Fireworks distinguishes model resources, deployments, and workload-specific
inference APIs. Chalk adopts that distinction as model, endpoint, and adapter
facts. Its capability-bearing fields live on List/Get Model, while deployment
constraints live on List/Get Deployment. It does not add a separate resolved
capability service. Nexus solves a different problem: it preserves a
coding-harness contract and ranks curated routes by task difficulty, cost, and
cache behavior. Chalk's judge and policy layers may rank only candidates that
first satisfy this RFP's hard capability requirements.

## 6. Terminology and support semantics

### 6.1 Facts, evidence, and effective capability

- **Model facts** describe the intrinsic model: surfaces, modalities, model
  features, and limits.
- **Endpoint facts** describe the configured provider connection: available
  upstream interfaces, regional/deployment restrictions, streaming, and
  accepted parameters.
- **Adapter facts** describe the code in Chalk: public interface operations,
  native forwarding, semantic adaptation, safe translation, and parameter
  conversion.
- **Evidence** is a sourced value used to resolve a fact.
- **Effective capability** is the public support guaranteed by the intersection
  of the three resolved fact layers.

### 6.2 Support state

Every support leaf uses:

```text
supported | unsupported | unknown
```

The intersection is:

| Left | Right | Result |
|---|---|---|
| supported | supported | supported |
| supported | unknown | unknown |
| supported | unsupported | unsupported |
| unknown | unknown | unknown |
| unknown | unsupported | unsupported |
| unsupported | any | unsupported |

Missing internal evidence resolves to `unknown`; it never silently becomes
`unsupported`. A resolved public profile emits explicit values for the axes it
supports rather than requiring clients to interpret omission.

### 6.3 Delivery mode

Chalk retains internal delivery modes:

```text
native | adapted | translated | unsupported | unknown
```

Delivery is a routing and diagnostic fact, not the public support contract. A
public operation is supported only when its request requirements are supported
and the adapter can serve them faithfully. Admin diagnostics expose delivery
mode by operation path.

### 6.4 Parameter constraints

Each operation parameter uses this version-1 tagged shape:

```json
{
  "support": "supported",
  "constraint": {
    "type": "string_enum",
    "values": ["1024x1024", "1536x1024"]
  }
}
```

`support` uses the same tri-state as other leaves. `constraint.type` is exactly
one of:

- `any`, meaning every value valid under the public request schema;
- `string_enum`, with a non-empty unique `values` array;
- `integer_range`, with inclusive integer `min` and `max`;
- `number_range`, with inclusive finite numeric `min` and `max`; or
- `unknown`, meaning the parameter itself is accepted but its value domain is
  not known.

`constraint` is required when support is `supported` and forbidden otherwise.
Enum intersection retains common values. Range intersection retains the
overlap. `any` is the identity element. An empty or type-incompatible
intersection makes the parameter unsupported and records a conflict. Adapter
code must validate or transform a parameter before advertising it as supported;
silent dropping does not count as support.

Each public operation owns a finite, code-reviewed set of capability-varying
optional parameters. The effective public profile materializes every parameter
in that set; missing evidence becomes `unknown`. A request field absent from the
set is governed by the ordinary public request schema and adapter validation,
not implicitly marked supported. Adding a new capability-varying parameter is
an additive schema-version-1 change.

## 7. Public capability schema, version 1

The canonical public shape is:

```json
{
  "schema_version": 1,
  "surfaces": {
    "chat": "supported",
    "embeddings": "unsupported",
    "image_generation": "unknown"
  },
  "modalities": {
    "input": {
      "text": "supported",
      "image": "unknown"
    },
    "output": {
      "text": "supported",
      "image": "unknown"
    }
  },
  "features": {
    "tool_calling": "supported",
    "structured_output": "unknown",
    "reasoning": "unknown"
  },
  "limits": {
    "context_window": 200000,
    "max_output_tokens": 8192
  },
  "interfaces": {
    "openai_chat_completions": {
      "operations": {
        "create": {
          "support": "supported",
          "features": {"streaming": "supported"},
          "parameters": {}
        }
      }
    },
    "anthropic_messages": {
      "operations": {
        "create": {
          "support": "supported",
          "features": {"streaming": "supported"},
          "parameters": {}
        },
        "count_tokens": {
          "support": "unsupported",
          "features": {},
          "parameters": {}
        }
      }
    }
  }
}
```

Canonical surfaces are chat, embeddings, image generation, rerank, moderation,
audio transcription, audio speech, and realtime. Canonical modalities remain
text, image, audio, video, PDF, and embeddings as applicable. Model features
remain distinct from operation features because a capable model can be reached
through an interface that cannot carry the feature.

Initial public interfaces and operations are:

```text
openai_chat_completions.create
openai_responses.create
openai_responses.retrieve
openai_responses.delete
openai_responses.compact
anthropic_messages.create
anthropic_messages.count_tokens
openai_embeddings.create
openai_images.generate
openai_images.edit
openai_images.variations
openai_audio.speech
openai_audio.transcription
openai_audio.translation
```

Only operations implemented by Chalk are emitted as supported. Model image
output alone cannot advertise `openai_images.generate`; the resolved endpoint
and Chalk adapter must also support that public operation.

Effective maximum limits use the minimum of applicable, known hard maxima.
Unknown limit evidence is omitted rather than serialized as zero, and a source
may explicitly state that it adds no further bound. Minimum limits use the
maximum. Invalid or empty limit intersections are conflicts and cannot be
published as supported.

## 8. Source resolution and provenance

Resolve each layer independently before intersection.

### 8.1 Model-fact precedence

1. Matching entry in `declared_capabilities.models`
2. `declared_capabilities.model_defaults`
3. Upstream model discovery
4. Curated exact-model registry entry
5. Curated documented-family registry entry
6. Conservative provider model default
7. Unknown

An explicit operator declaration may correct stale registry or discovery facts.
Conflicting lower-priority evidence remains available to internal tests and
bounded diagnostics.

### 8.2 Endpoint-fact precedence

1. `declared_capabilities.endpoint`
2. Upstream endpoint/deployment discovery
3. Connection-shape facts such as Bedrock Mantle versus Runtime
4. Provider endpoint defaults
5. Unknown

### 8.3 Adapter facts

Adapter facts are code-owned and are not operator-overridable. A declaration can
describe a custom upstream endpoint, but cannot claim Chalk implements a public
operation or translation that its adapter does not implement.

### 8.4 Multi-source provenance

The effective result of an intersection can have more than one source. Replace
the current one-string-per-leaf provenance assumption with repeated evidence
records in the internal resolver. Use typed path segments rather than
dot-delimited strings because canonical model ids can contain dots and slashes.
Public model responses omit provenance. No public or admin contract exposes the
internal evidence representation in this change.

## 9. Canonical model identity and registry

Registry lookup uses `(provider_kind, canonical_upstream_model_id)`. Remove the
configured Chalk prefix exactly once and preserve meaningful nested ids:

```text
openai/gpt-image-2
  -> (openai, gpt-image-2)

gemini/models/gemini-3-pro-image
  -> (gemini, models/gemini-3-pro-image)

openrouter/openai/gpt-5
  -> (openrouter, openai/gpt-5)
```

Public `chalk/*` routes resolve to their configured connection and upstream
model before lookup. Unknown prefixes fail strict resolution where the ingress
already requires strict resolution; capability lookup never guesses a provider
from an arbitrary substring.

The registry is table-driven. Each rule contains:

- provider kind;
- exact id or documented family matcher;
- sparse model facts;
- optional endpoint restrictions when the provider documents them per family;
- a stable rule name; and
- a documentation/review note.

Matching order is exact id, most-specific family rule, provider default, then
unknown. Tests fail on ambiguous same-priority matches.

Initial coverage is intentionally bounded to configured and user-visible model
families, including OpenAI image/embedding/audio/reasoning models, Gemini and
Imagen image/embedding families, Anthropic Claude families, Bedrock model
variability, Voyage embeddings, and existing routed providers. OpenRouter and
Fireworks prefer their richer discovery data instead of duplicating their full
catalogs.

Generic regular expressions such as `/image/i` and
`/embed|ada|voyage/i` are not registry truth. They may identify an unverified UI
candidate only.

## 10. Discovery and shared capability catalog

Provider adapters return an internal catalog entry containing the ordinary
model object plus an optional sparse discovery patch. Do not deserialize a
provider's unrelated `capabilities` field directly into Chalk's public schema.

Initial discovery mappings are:

- OpenRouter: input/output modalities, context length, supported parameters,
  and endpoint-specific records when available;
- Fireworks: model kind, conversation configuration, context length, image
  input, tools, serverless/deployment availability, and workload type;
- Bedrock: foundation-model input/output modalities, streaming, region
  availability, and documented Converse/Mantle facts available from its control
  plane; and
- generic OpenAI-compatible endpoints: only fields conforming to a recognized
  schema, otherwise unknown.

OpenAI and Gemini continue relying on curated facts where their ordinary model
lists lack sufficient metadata.

Discovery is cached by connection identity/fingerprint and canonical upstream
model id. A shared in-memory capability catalog combines discovery with
registry, declarations, and adapter facts. `list_models`, `get_model`, published
route projection, and request resolution call the same resolver over that
catalog. No request-path provider call is introduced. Cold resolution always
has declarations, registry facts, connection shape, and adapter facts; discovery
refines it when present.

Stale-while-revalidate behavior may serve stale discovery while refreshing.
Changing a connection declaration invalidates effective profiles. Transport
changes such as Bedrock Mantle versus Runtime remain part of the connection
fingerprint. A code deployment naturally changes registry and adapter facts;
effective cache keys must include a code-owned catalog revision if effective
profiles themselves are cached.

## 11. Runtime requirements and enforcement

Every handler derives requirements from its public interface, operation, and
actual body before dispatch. Requirements include:

- surface;
- input and output modalities;
- model features;
- operation features such as streaming;
- stateful operations and continuation;
- tools and server-side tools; and
- present parameter values and constraints.

Initial enforcement remains compatible with RFP-0004:

- explicit unsupported rejects locally before dispatch;
- supported dispatches;
- unknown remains attemptable and emits a diagnostic signal;
- translated and adapted paths reject non-representable fields rather than
  silently dropping them; and
- API-key permission is evaluated independently.

Strict unknown rejection is a separate future policy decision. A failed
upstream request does not mutate capability truth. Upstream errors can arise
from authentication, quota, region, policy, or transient health.

## 12. Protocol crossing and delivery

The following combinations remain required and receive explicit contract tests:

| Public ingress | Destination | Delivery |
|---|---|---|
| Chat Completions | Anthropic Claude | adapted to Messages |
| Anthropic Messages | OpenAI chat model | translated safe subset |
| Responses create | chat-only model | translated safe subset |
| Anthropic Messages | Vertex Claude | adapted |
| Messages/Responses | Bedrock Runtime | adapted through Converse |
| Provider-native interface | eligible model on eligible endpoint | native |

Messages-to-Chat supports only the documented lossless subset. `count_tokens`
is independent and is not translated through Chat. Responses create is
independent from retrieve, delete, compact, continuation, and background.

OpenRouter's Messages operation is resolved per nested model and endpoint; the
provider kind alone no longer makes every `openrouter/*` model Messages-native.
Qualified model ids or published routes are required for cross-provider ingress.
Existing ingress-specific behavior for bare ids remains unchanged.

## 13. Public aliases and fallback

A published `chalk/*` route advertises the effective profile of its primary
target. It never advertises the union of fallback capabilities. Before each
fallback attempt, resolve that candidate and compare the concrete request
requirements with its effective profile. Incompatible candidates are skipped
without replacing the last real dispatch error, as required by RFP-0002.

Routing policy is evaluated only after capability eligibility. Price, latency,
throughput, cache affinity, task difficulty, and judge score may rank eligible
candidates but cannot make an ineligible candidate eligible.

## 14. Playground and client projections

Each Playground tab filters by a complete requirement, not by provider or an
exclusive model family:

- Chat requires the chat surface plus a supported generation operation.
- Embeddings requires the embeddings surface, text input, embeddings output,
  and `openai_embeddings.create`.
- Image Generation requires the image-generation surface, text input, image
  output, and `openai_images.generate`.

A model may appear in more than one tab. Remove `filterOrAll`; an empty verified
set must not offer every model as if it were compatible.

The UI presents supported models normally and may present unknown,
name-matched candidates in a separate **Unverified** group. Generic name
heuristics never change server capability truth. Manual model entry remains
available for advanced testing. Unsupported models remain hidden for that tab.

Operation parameter constraints control relevant UI inputs. Size, quality,
count, aspect ratio, and similar controls show only values supported by the
selected effective operation; the Playground does not send one provider's
parameters blindly to every model.

VS Code and future projections are derived outputs. Unknown projects
conservatively to false where a client supports only booleans.

## 15. Admin UI

The provider list stops displaying a connection-level effective model profile.
It continues to edit the advanced schema-version-1 declaration and explains its
three scopes: model defaults, per-model overrides, and endpoint facts.

Do not add a model-specific diagnostics panel or RPC. Effective user-visible
capabilities belong on the existing model list/detail surfaces. The Models UI
and Playground consume those surfaces; the provider editor shows only the raw
declaration the operator can change. Internal evidence and delivery mechanics
remain implementation details until a concrete debugging workflow justifies a
separate read-only contract.

The admin UI must not imply that connection-test success is durable capability
evidence. No automatic or billable Detect action is added by this RFP.

## 16. Errors, observability, and drift

Local capability errors name the model, operation, and failed requirement, for
example:

```text
Model 'openai/gpt-image-2' cannot serve openai_images.generate:
parameter 'size' value '1792x1024' is unsupported.
```

Add bounded diagnostic fields or metrics for:

- requested public and resolved upstream model;
- required interface operation;
- effective support decision;
- delivery mode;
- whether unknown was attempted;
- registry rule and high-level evidence source; and
- upstream errors indicating an advertised capability mismatch.

Do not log full capability documents on every request and do not add unbounded
model ids as metric labels where the telemetry backend cannot tolerate them.
Existing OpenInference content and token budgets remain unchanged.

An upstream rejection of an advertised supported operation increments a
mismatch diagnostic and remains an ordinary request failure. It does not
rewrite declarations, discovery, or registry data automatically.

## 17. Implementation plan: one PR, separate commits

All work in this RFP lands in one pull request so the schema, generated clients,
resolver, runtime behavior, and UI cannot ship in contradictory states. The PR
must retain the following independently reviewable commits; combining them into
one implementation commit is not acceptable.

1. **RFP and contract fixtures.** Add this RFP and failing schema/registry
   fixtures that define the intended version-1 contract.
2. **Persistence and protobuf cleanup.** Reshape `CapabilityPatch`, retain the
   JSONB column and schema version, remove connection-level effective fields and
   `CapabilityConflict` without reserving their field numbers, regenerate
   protobuf code, make declaration decoding strict and error-propagating, add
   the non-null-row preflight, and update seed/admin serialization.
3. **Model identity and curated registry.** Add canonical upstream identity,
   exact/family rules, ambiguous-match validation, and initial OpenAI, Gemini,
   Anthropic, Bedrock, Voyage, and existing-provider coverage.
4. **Layered resolver and shared catalog.** Separate model, endpoint, and adapter
   facts; implement precedence, intersection, parameter constraints, evidence,
   conflicts, caching, and list/detail/runtime consistency.
5. **Provider discovery.** Map OpenRouter, Fireworks, Bedrock, and recognized
   OpenAI-compatible metadata into ephemeral discovery patches without adding
   request-path I/O.
6. **Runtime requirements and protocol audit.** Extend interface operations,
   extract concrete request requirements, correct Bedrock and OpenRouter model
   variability, preserve safe protocol crossing, and keep fallback semantics.
7. **Public and client projections.** Publish the new effective `/v1/models`
   and model-detail shape, remove connection-level admin projections, and
   migrate the Models UI and VS Code projection.
8. **Playground.** Replace modality heuristics with effective requirements, add
   unverified/manual behavior, and constrain operation-specific controls.
9. **Observability, tests, and documentation.** Add mismatch diagnostics,
   complete integration/frontend coverage, update RFP-0001/RFP-0004 references,
   and add the user-visible changelog.

Each commit should compile and keep generated sources synchronized where
practical. The final PR gate is `just ci`. Paid live tests remain opt-in and are
not required for the normal gate.

## 18. Testing strategy

### 18.1 Schema, declaration, and protobuf

- Version-1 serde round trips for the new declaration and public shapes.
- Protobuf tests verify `declared_capabilities` remains and the three
  connection-level derived fields plus `CapabilityConflict` are removed.
- Reject the old pre-rollout shape, unknown keys, and future versions.
- Field-mask inherit, replace, and clear behavior.
- Per-model override keys use canonical upstream ids.
- Public model responses omit internal evidence and delivery mechanics.
- No capability-resolution service is present in the generated gRPC surface.

### 18.2 Resolver

- Full support-state intersection truth table.
- Enum and numeric-range constraint intersection.
- Per-axis precedence and explicit operator correction.
- Multi-source evidence and conflict diagnostics.
- Exact versus family registry matching and ambiguous-match failure.
- Bedrock Runtime connection shape is identical in list and runtime resolution.

### 18.3 Required model fixtures

At minimum:

- `openai/gpt-image-2`;
- OpenAI `text-embedding-*`;
- `gemini/models/gemini-3-pro-image`;
- current Gemini embedding families;
- `bedrock/anthropic.claude-haiku-4-5`;
- `openrouter/openai/gpt-*`;
- `openrouter/anthropic/claude-*`; and
- a custom unknown OpenAI-compatible model.

### 18.4 Runtime and fallback

- Chat to Anthropic adaptation, non-streaming and streaming.
- Messages to OpenAI translation for text, images, tools, results, and stream;
  reject non-representable fields before dispatch.
- Responses-to-Chat safe translation and independent stateful operations.
- Unsupported count-tokens combinations reject locally.
- Concrete parameter violations reject locally.
- Unknown remains attemptable.
- Incompatible fallback is skipped without masking the last dispatch error.

### 18.5 Discovery

Use checked-in mock provider payloads for OpenRouter, Fireworks, Bedrock, and a
generic OpenAI-compatible endpoint. Verify stale discovery, connection
fingerprint invalidation, and cold registry-only resolution. CI performs no
paid provider call.

### 18.6 UI

- Both reported image models appear when their complete image requirements are
  supported.
- Embedding models appear from effective facts.
- Unknown candidates are labeled unverified.
- Unsupported models are hidden.
- Multi-surface models appear in every applicable tab.
- An empty verified set does not fall back to every model.
- Changing model changes parameter controls without leaving invalid values in
  the request.

## 19. Rollout and compatibility

The capability contract is pre-rollout, so this RFP deliberately makes a clean
version-1 correction with no dual-read, dual-write, compatibility alias, or
schema-version bump. Generated clients, backend, admin UI, Playground, tests,
and documentation land atomically in one PR.

Runtime enforcement does not become stricter for unknown facts. The immediate
behavioral changes are corrections of explicit false promises, capability-aware
model presentation, and validation of operation parameters that Chalk claims to
support.

If any environment unexpectedly contains a non-null old-shape declaration, the
preflight must stop the deployment with an actionable validation error naming
the connection; the read path independently returns a typed error. Neither path
may silently discard or reinterpret operator intent.

## 20. Risks and mitigations

- **Registry drift:** prefer provider discovery where authoritative, attach
  stable rule names and review notes, and record upstream mismatch diagnostics.
- **Overengineering:** bound initial registry coverage and do not add catalog
  queries, persisted discovery, pricing policy, or automated probes.
- **False positives from names:** generic heuristics are UI-only and unverified;
  server facts use documented provider-specific rules.
- **False negatives from unknown:** unknown remains visible and attemptable;
  manual Playground entry remains available.
- **Mixed catalogs:** connection JSON supports sparse per-model overrides without
  a new table or shared mutable resource.
- **Public/dispatch divergence:** one shared catalog and resolver serve model
  list, detail, routes, fallbacks, and dispatch.
- **Translation drift:** operation features and parameters describe the safe
  subset; codecs reject everything else before upstream dispatch.
- **Catalog staleness:** discovery is ephemeral, fingerprinted, and
  stale-while-revalidate; registry and declarations provide cold-start facts.
- **Telemetry cardinality:** detailed evidence stays internal; request telemetry
  exposes only bounded source categories and support decisions.

## 21. Acceptance criteria

This RFP is complete when all of the following are true:

1. No provider-wide profile is published as the effective capability of every
   model on that provider.
2. `/v1/models`, model detail, client projections, and runtime dispatch agree
   for the same resolved target.
3. `openai/gpt-image-2` and `gemini/models/gemini-3-pro-image` are correctly
   classified and appear in the image Playground only when Chalk can serve the
   required image operation.
4. Bedrock Claude Haiku is not sent to an unsupported native Responses
   operation; it uses a supported adapted/translated path or rejects locally.
5. OpenRouter Messages capability is resolved per nested model and endpoint,
   not from provider kind alone.
6. Model, endpoint, and adapter facts are distinct and intersected.
7. Operation parameters are constrained where known and never silently dropped.
8. Unknown is distinct from unsupported in APIs, UI, routing, and tests.
9. Connection declarations support defaults, sparse per-model corrections, and
   endpoint facts in the existing JSONB column with `schema_version: 1`.
10. ProviderConnection no longer claims one effective model profile, and no
    standalone resolver/provenance RPC is introduced. The later
    connection-scoped model catalog is a derived public-profile projection.
11. Old or invalid declarations fail preflight and database reads rather than
    degrading to an inherited profile.
12. Public aliases do not advertise a union of fallback capabilities.
13. Capability eligibility remains separate from authorization, health, price,
    performance, and judge policy.
14. The implementation introduces no capability table, persisted discovery, or
    automatic paid probe. A later admin change may add a bounded catalog
    projection only if it reuses the same effective resolver.
15. The one implementation PR contains the separate commits required by
    section 17 and passes `just ci`.

## 22. Decisions

1. The corrected contract remains capability schema version 1.
2. There is no new DDL; declarations remain in the existing connection JSONB.
3. Sparse per-model operator corrections are embedded in the connection
   declaration rather than stored as independent resources.
4. Raw provider/connection defaults are not public effective model truth.
5. Public model list/detail objects expose effective support. Provenance,
   conflicts, discovery freshness, registry rules, and delivery modes remain
   internal.
6. Model, endpoint, and adapter facts resolve independently and intersect.
7. Generic name heuristics never produce supported server capability facts.
8. Capability eligibility is a hard constraint; routing policy ranks only
   eligible candidates.
9. Unknown remains attemptable during this change.
10. The implementation lands atomically in one PR with separate reviewable
    commits.
11. Protobuf field numbers 12 through 14 are not reserved because the removed
    contract never rolled out.

## 23. References

- [OpenRouter model catalog](https://openrouter.ai/docs/guides/overview/models)
- [OpenRouter model endpoint records](https://openrouter.ai/docs/api/api-reference/endpoints/list-endpoints)
- [OpenRouter Anthropic Messages API](https://openrouter.ai/docs/api/api-reference/anthropic-messages/create-messages)
- [OpenRouter provider routing](https://openrouter.ai/docs/guides/routing/provider-selection)
- [OpenRouter image endpoint capabilities](https://openrouter.ai/docs/guides/overview/multimodal/image-generation)
- [LiteLLM model catalog schema](https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.schema.json)
- [LiteLLM model-group capability example](https://docs.litellm.ai/docs/completion/audio)
- [OpenAI GPT Image 2 model contract](https://developers.openai.com/api/docs/models/gpt-image-2)
- [Fireworks model resources](https://docs.fireworks.ai/models/overview)
- [Fireworks list-models API](https://docs.fireworks.ai/api-reference/list-models)
- [Fireworks model API](https://docs.fireworks.ai/api-reference/get-model)
- [Fireworks deployment API](https://docs.fireworks.ai/api-reference/get-deployment)
- [Fireworks Nexus](https://fireworks.ai/nexus)
- [Amazon Bedrock foundation-model metadata](https://docs.aws.amazon.com/bedrock/latest/userguide/inference-api.html)
