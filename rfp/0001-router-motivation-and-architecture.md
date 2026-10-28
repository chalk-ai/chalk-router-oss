# RFP-0001: chalk-router — Motivation and Architecture

> **Reading note (2026-10-26):** This record preserves the architecture and
> rationale at its revision dates. Later changes include gRPC-only administration,
> cluster tenancy, route-owned judging/fallback, and expanded API support. Use
> [current guides](../docs/README.md) for operations, the
> [handler pipeline table](../src/handlers/mod.rs) for enforcement, and the
> [authentication transition](../docs/authentication-transition.md) for unmerged
> security requirements. Statements below about HTTP admin or a single environment
> are historical, not the current setup contract.

- **Status:** Accepted / Implemented (living architecture reference)
- **Created:** 2026-07-23
- **Revised:** 2026-09-03 — refreshed usage accounting, API-key scopes,
  quota enforcement, control-plane services, and Helm deployment topology
- **Supersedes:** `docs/design/provider-connections-and-model-routes.md` (folded into §7)
- **Related RFPs:**
  [RFP-0002 Streaming fallback](0002-streaming-fallback.md) (built) ·
  [RFP-0003 Permission scope for non-LLM endpoints](0003-permission-scope-non-llm-endpoints.md) (audio deferred) ·
  [RFP-0004 Per-model capability profiles](0004-per-model-modality.md) (built; partly superseded) ·
  [RFP-0005 Effective per-model capabilities](0005-effective-per-model-capabilities.md) (built) ·
  [RFP-0006 Usage attribution and token accounting](0006-usage-attribution-and-cost-accounting.md) (historical) ·
  [RFP-0007 Backend-neutral quota enforcement](0007-backend-neutral-quota-enforcement.md) (built)

---

## 1. Motivation

**chalk-router** is an LLM proxy/router (Rust + Axum) that exposes **OpenAI-** and
**Anthropic-compatible** HTTP APIs and fans requests out to many upstream
providers (OpenAI, Anthropic, Google Gemini and Vertex, xAI/Grok, Bedrock,
OpenRouter, Cohere, Qwen, Fireworks, Together, Groq, Mistral, Voyage, and
Chalk-hosted vLLM).
Alongside the proxy it serves an admin/management surface (HTTP + gRPC/gRPC-Web)
and a Next.js admin UI.

It exists to solve problems that appear once *more than one* application at Chalk
needs to talk to *more than one* LLM provider:

- **One API surface, many providers.** Callers speak the OpenAI (or Anthropic)
  wire format and address any provider by a model prefix (`gemini/…`,
  `anthropic/…`) or a stable Chalk alias (`chalk/…`). Swapping the upstream behind
  an alias needs no client change.
- **Centralized credentials.** Provider API keys live in one place —
  encrypted at rest in the database — instead of being copied into every app's
  environment. Keys can be rotated at runtime without a redeploy, and no provider
  is reachable until a credential for it is stored.
- **Governance.** Per-caller API keys carry provider/model allowlists, usage-pool
  membership, cost tags, and budgets; usage is metered and budgets enforced at the
  gateway rather than reimplemented per app.
- **Observability.** Every request emits an OpenInference-conforming trace, and
  clients can stitch their own agent spans into the router's LLM spans via a span
  ingest endpoint — one coherent trace per session.
- **Multi-tenancy.** One deployment serves exactly one Chalk *environment*; many
  environments can share a Postgres instance with hard `environment_id` isolation.
- **Federated identity.** Chalk JWTs are accepted on both the admin and proxied
  surfaces, so `api-server-go` can forward a user's token straight through without
  provisioning a router-specific key.
- **Resilience.** A fallback policy maps a requested model to ordered alternates,
  and stable `chalk/*` aliases decouple the public model name from the upstream
  that serves it.

## 2. How we got here (evolution)

The system grew feature-first and was periodically consolidated. Abridged from
git history:

1. **Proxy core** — an OpenAI-compatible `/v1/chat/completions` with SSE
   streaming, then an Anthropic `/v1/messages` pass-through (for Claude Code), then
   images/speech/audio and a `/v1/debug/completions` sampler.
2. **Governance** — synthetic API keys with per-key token quotas, a model
   fallback policy, then an admin UI (keys, usage, providers, playground).
3. **Persistence** — Postgres for API keys and per-provider usage timeseries;
   fallback policy moved from env to the database; daily budgets, rate-limit
   policies, and usage pools ("teams") added.
4. **Two wire formats** — a gRPC layer added alongside HTTP with a shared service
   layer, so the admin plane is consistent regardless of transport.
5. **Federation & runtime credentials** — federated Chalk JWTs on both surfaces;
   provider credentials moved from env vars to database-backed connections.
6. **Consolidation (Phase 1)** — a dedicated `chalk_router` Postgres schema, hashed API
   keys, usage pools, `environment_id` tenancy scoping, a `migrate` subcommand, and
   a pluggable usage ledger and shared quota runtime (plain Postgres implemented).
7. **Encryption at rest** — provider credentials encrypted with AES-256-GCM.
8. **Provider connections & model routes (current)** — the built-in provider
   registry, `provider_connections`, and `chalk/*` `model_routes` described in §7,
   replacing the last hardcoded provider paths. This RFP is written against that
   state.
9. **Capability-aware protocols** — versioned capability profiles, native and
   translated Responses operations, broader Anthropic ingress, Bedrock Runtime,
   and canonical xAI routing described in RFP-0004.

## 3. System overview

Two planes served by **one** axum process on **one** port (default `4001`):

- **Data plane** — the OpenAI/Anthropic-compatible `/v1/*` proxy endpoints.
- **Control plane** — the admin/management surface, exposed *twice*: HTTP/JSON
  (`/v1/admin/*`, back-compat; removed 2026-09-10, gRPC is the only admin
  surface) and gRPC + gRPC-Web (`chalk.router.v1.*`). The
  tonic services are wrapped with `tonic_web::GrpcWebLayer` and `.merge()`-d into
  the same axum router (`src/grpc/router.rs`, `src/main.rs`), so both transports
  share the port, CORS, and auth middleware.

Both wire layers are thin adapters that parse/validate and then dispatch into a
single shared **service layer** (`src/service/`), so request semantics stay
identical across HTTP and gRPC. HTTP handlers live in `src/handlers/`; gRPC
service impls in `src/grpc/`; upstream provider clients and routing in
`src/backend/`.

```
                       ┌──────────────────────────────────────────┐
   OpenAI/Anthropic    │  axum :4001                               │
   SDK clients ───────▶│  /v1/* data plane   ── handlers/ ──┐      │
                       │  /v1/admin/* ─┐                     │      │
   Admin UI (gRPC-web)─▶  chalk.router.v1.* (tonic-web) ─┐   │      │
                       │                │                 ▼   ▼      │
                       │           service/  ◀── shared ── backend/ │──▶ upstream providers
                       │                │                 │         │
                       │        stores (Postgres | in-mem)│         │──▶ OTLP (traces)
                       └──────────────────────────────────┘─────────┘
```

Deployed through a commit-pinned Helm release on GKE as independently scalable
backend and UI Deployments (§13).

## 4. Request surface

**Data plane (proxied, implemented)** — forward to the resolved upstream (§7):
`POST /v1/chat/completions` (streaming, tools, structured output);
`POST /v1/responses`, `/v1/responses/compact`, and
`GET`/`DELETE /v1/responses/:id` (native or losslessly translated);
`POST /v1/messages`, `/v1/messages/count_tokens` (native, adapted, or translated);
`POST /v1/embeddings`; `POST /v1/images/generations`;
`GET /v1/models`, `GET`/`DELETE /v1/models/:id`;
`POST /v1/audio/speech|transcriptions|translations`;
`POST /v1/traces/spans` (OpenInference span ingest, §10).

**Control plane (admin-gated)** — HTTP `/v1/admin/*` and gRPC `chalk.router.v1.*`:
`ApiKeyService`, `UsagePoolService`, `UsageService`, `UsageBudgetService`,
`RateLimitService`, `FallbackPolicyService`, `JudgePolicyService`,
`ProviderConnectionService`, `ModelRouteService`, `TelemetrySettingsService`,
and `DebugService`. HTTP mirrors cover the admin UI's key, usage, pool, budget,
rate-limit, routing, provider, judge, and telemetry workflows plus
`/v1/debug/completions`.

**Health (public):** `GET /health`, `GET /v1/healthcheck`, and standard gRPC
`grpc.health.v1.Health`.

**Stubbed / partial:** Assistants, Files (partial), Fine-tuning, Moderations,
Batches, `POST /v1/images/edits|variations` (`501`). They model the vendor API
shape but are not implemented.

## 5. Authentication & authorization

The single `auth_middleware` (`src/middleware.rs`) wraps the merged router and
classifies each path:

- **Skip** (`/health`, `/v1/healthcheck`, gRPC `grpc.health.v1.Health`) — public.
- **Admin** (`/v1/admin/*`, `/v1/debug/*`, every `chalk.router.v1.*` gRPC service) —
  requires `ROUTER_ADMIN_KEY` (verbatim bearer match) **or** a valid federated Chalk JWT.
- **Proxied** (everything else) — requires a stored API key, admin key, federated
  JWT, or nothing when `ROUTER_ALLOW_UNAUTHENTICATED=true` (the current demo deployment).

**Client API keys** (`src/api_keys.rs`, `src/hashing.rs`): stored as
`HMAC-SHA256(raw_key, ROUTER_KEY_PEPPER)` hex — never plaintext. HMAC is
deterministic, so lookup by `key_hash` stays O(1). `create()` mints `ck-{uuid}`,
persists the hash first, and returns the raw secret **once**; only the non-secret
`key_prefix` (`ck-1a2b3c4d`) is ever shown again. Stored keys carry `permission`,
`usage_pool_id`, and immutable create-only `labels`. The deprecated `cost_tags`
wire fields are returned empty and non-empty inputs are rejected.

**Permission model** (`ApiKeyPermission`): the current authoring surface uses
`connection_scopes`, whose entries pair one immutable `connection_id` with an
optional list of public model ids. Empty models means every model on that
connection. The legacy flat `provider`, `connection_id`, and `models` axes remain
readable and are AND-ed with `connection_scopes` when present. `is_restricted()`
is false when every allowlist form is absent (unrestricted keys skip enforcement
and the route resolve entirely). The predicate
`allows(provider_kind, connection_id, public_model_id)`:
- `provider` must equal the resolved **provider kind** (e.g. `openai`,
  `openai-compatible`). It is matched against the kind **only** — never a
  connection name — so a connection an operator happens to name `openai` can't
  satisfy a key restricted to the real `openai` provider;
- `connection_id` (when set) must equal the **immutable id** of the connection
  the request resolved through. This is how a key is scoped to one specific
  endpoint (e.g. a single `openai-compatible` connection, which all share one
  kind); keying on the id rather than the mutable name means a rename can't
  transfer the grant, and a route that resolves without a stored connection is
  denied;
- `models` must contain the **public** model id attempted (the alias, e.g.
  `chalk/gemma`, not the upstream id).
- `connection_scopes` must contain the resolved connection, and that entry's
  model list must be empty or contain the public model id attempted.

The same permission document also carries judge opt-in/floor and prompt-cache
settings. Those fields configure request behavior; they are not authorization
allowlist axes.

It keys off the resolved `RouteResolution`, not the raw model string, so the same
check applies to the primary and each fallback candidate. It is enforced before
dispatch in Chat Completions and Responses (primary + per-fallback +
streaming-before-200), Anthropic Messages/count-tokens, embeddings, and images.
Audio permission enforcement is explicitly deferred; moderation is still a local
stub rather than an upstream dispatch (see [RFP-0003](0003-permission-scope-non-llm-endpoints.md)).

**Admin & federated JWT:** `ROUTER_ADMIN_KEY` is a static bearer token. `CHALK_JWT_ISSUERS`
is a comma-separated list of trusted issuers, each either a discovery URL or
`iss=jwks_uri` for bare-string issuers; JWTs are validated against the issuer's
JWKS (cached ~300s, no shared secret) via the `chalk-federated-auth` crate, with
optional `aud` pinning (`CHALK_JWT_AUDIENCE`). Admin/JWT users on the proxied
surface get a synthetic **unrestricted** `ApiKey` (`__admin__` / `federated:{sub}`),
so permission gating is bypassed for them.

## 6. Multi-tenancy

`environment_id` (`CHALK_ENVIRONMENT_ID`) is the tenant-isolation boundary: one
process serves one environment; a shared Postgres instance may host many. It is
**required (fail-fast) whenever a database is configured**, read once at boot, and
threaded into every Postgres store, which filters **every** query by it
(INSERT binds it; SELECT/UPDATE/DELETE all `WHERE environment_id = $1`). The
`ROUTER_KEY_PEPPER` is per-environment too, so `key_hash` values are disjoint
across tenants. Runtime and migration SQL schema-qualify `chalk_router.*`; database
URL startup `options` are stripped and `PGOPTIONS` is rejected, so a shared
database's inherited `search_path` cannot redirect router queries.

> Isolation rests on the per-query filter + per-environment pepper. The
> federated-JWT `environment_id` guard is currently **opt-in**: a token lacking the
> claim is accepted regardless of environment (`src/middleware.rs`). A token that
> names a *different* environment is rejected unless
> `ROUTER_ACCEPT_CROSS_ENVIRONMENT_TOKENS` is set, which the shared demo
> instance uses so go-api-server can forward caller JWTs without rewriting
> `environment_id`. Tightening claimless tokens is a candidate hardening follow-up.

## 7. Model routing & provider connections

This section folds in and supersedes the former
`provider-connections-and-model-routes.md`. Four concerns are kept separate:

### 7.1 Built-in provider registry (`src/provider_registry.rs`)

A single code-owned table mapping each `provider_kind` → adapter, canonical
prefix (`openai/`, `anthropic/`, …), default endpoint (or `None` where the adapter
owns it), display name, and conservative capability defaults. The capability
profile is resolved per model as specified by RFP-0004; it is not inferred from
the display name or adapter kind. Built-in kinds: `openai`, `anthropic`, `gemini`,
`vertex`, `voyage`, `vllm`, `bedrock`, `openrouter`, `xai`, `cohere`, `qwen`,
`fireworks`, `together`, `groq`, `mistral`. The former `grok` provider kind and
prefix were removed in favor of canonical `xai`. The pseudo-kind `openai-compatible` is
deliberately **not** in the registry: it has no default endpoint (a `base_url` is
required) and no canonical prefix (each connection supplies its own). Every kind
may have multiple connections. There is no env-var credential loader —
credentials come only from stored connections.

### 7.2 Provider connections (`src/provider_connections.rs`, `chalk_router.provider_connections`)

A credential + endpoint config. Every kind may have multiple rows; the composite
primary key is `(environment_id, id)`, while names and non-null prefixes each
remain unique within an environment. Built-ins may use either their canonical
prefix or a custom one. The API key is **encrypted at rest** in a single
`api_key_envelope` column (§8). Blank
`base_url` overrides are normalized to `None` at the service boundary so a built-in
falls back to its registry default rather than a literal empty endpoint. A read
distinguishes three states so an unreadable ciphertext is never confused with an
absent one: `None` (no row), `Readable` (decrypted), `Unreadable` (row exists but
ciphertext won't decrypt — surfaced, never silently substituted). A keyless
upstream (unauthenticated vLLM, seeded Chalk endpoint) stores an encrypted
`KEYLESS_PLACEHOLDER = "unused"`.

### 7.3 Model routes (`src/model_routes.rs`, `chalk_router.model_routes`)

Environment-scoped `chalk/*` aliases mapping a public model id → a connection id +
an upstream model id. `connection_id` is a **soft reference** (no FK by design): a
route may point at a since-deleted connection; dangling refs surface as
diagnostics, not write-time errors. `public_model_id` is immutable and unique
per environment.

### 7.4 Resolution order and the resolve-once contract

`RouterBackend` (`src/backend/router.rs`) resolves a request `model` in order:
1. **`chalk/<id>`** — an exact **published** route → its connection + upstream
   model. `chalk/` is reserved and route-only, so an unknown `chalk/*` never falls
   through to OpenAI.
2. **`<prefix>/…`** — the connection owning `<prefix>/` (a built-in canonical
   prefix or a bespoke one); the prefix is stripped.
3. **Catch-all** — anything else (bare name, or unknown prefix) → the eligible
   `openai` connection that owns `openai/`, model unchanged. If there is no
   canonical owner, the sole eligible OpenAI connection is selected for backward
   compatibility. Multiple eligible non-canonical connections are an actionable
   configuration error. `explicit` connections are never eligible. The
   Anthropic-native entrypoint strictly resolves qualified prefixes and
   published aliases through steps 1–2, while bare provider-native IDs use the
   same canonical-or-sole Anthropic selection rule.

A request resolves the route **exactly once** into a `ResolvedRoute { backend,
stripped_model, resolution }` and carries that stable triple through *permission →
dispatch → attribution*. Dispatch goes straight to the leaf `backend` (never back
through the router, which would re-resolve against a possibly-shifted snapshot).
Two variants exist: lenient `resolve` (client requests, catch-all included),
strict `resolve_strict` (fallback alternates — an unresolved alternate is an error,
never a silent OpenAI passthrough), and `resolve_default` (the OpenAI default for
model-less requests, e.g. image generation, authorized against the real
connection identity). An `Unreadable` connection **hard-fails** the attempt.

### 7.5 Caches

- **Resolution snapshot** (5s SWR): all resolution reads go through an in-memory
  snapshot of both stores (indexed `by_prefix`/multi-valued `by_kind`/`by_id`/
  routes), rebuilt
  at most once per 5s with single-flight and serve-stale-on-error. Steady-state
  routing does ~0 DB queries and survives a DB blip. A credential rotated at
  runtime takes effect within the TTL, no restart.
- **Backend cache**: built HTTP backends are memoized by connection id and rebuilt
  when the connection's **fingerprint** over `(api_key, base_url)` changes — a
  collision-free, length-prefixed encoding that also distinguishes `None` from an
  empty base URL. Backends for connections absent from a refreshed snapshot are
  evicted (no leak on delete).
- **Model discovery cache** (300s SWR + single-flight): per-prefix `/v1/models`
  catalogs, tagged with the producing connection's identity (id + fingerprint) so
  a rotated/replaced connection is treated as cold rather than served a stale
  catalog. Fresh served lock-free; stale served immediately + one background
  refetch; cold fetches coalesce behind one shared future (a failed cold fetch is
  not cached). Catalogs are never persisted, so a restart is a cold miss.

### 7.6 Exposure, listing, deletes, diagnostics, fallback, seeding

- **Exposure**: `dynamic` (callable + listed in `/v1/models`), `unlisted`
  (callable by prefix, hidden from listing), `explicit` (route-only, no prefix).
  `routing_enabled` is an independent kill switch.
- **`/v1/models`** = union of `dynamic`+readable+enabled connections' live catalogs
  (prefix re-added) + published `chalk/*` routes (`owned_by: "chalk"`).
- **Delete**: `DELETE /v1/models/:id` deletes an exact `chalk/*` route (with
  warning-only dangling-fallback references) → `deleted:true`; a discovered model
  is a no-op (`deleted:false`); unknown → `404`.
- **Route diagnostics**: `ready` / `unconfigured` (readable+enabled but missing a
  required `base_url`, i.e. vllm/openai-compatible) / `disabled` / `unreadable` /
  `missing_connection`.
- **Fallback policy** (`chalk_router.fallback_policy`, one JSONB row/env): maps a public
  model to ordered public alternates; each alternate is resolved **strictly** and
  permission-filtered, then re-dispatched; unresolved/denied/disabled alternates
  are skipped. One level deep, and applies to **both non-streaming and streaming
  chat** — for streaming, only *before first byte*: a candidate whose stream fails
  to open is retried transparently, but once the `200` is committed a later failure
  stays in-band (see [RFP-0002](0002-streaming-fallback.md)). Every failure returned
  by an actual upstream dispatch advances an explicitly configured cross-model
  chain, regardless of HTTP status. Router-owned decisions made before dispatch
  remain terminal (primary resolution or permission) or skipped (an unresolvable,
  denied, disabled, or capability-ineligible alternate). A skipped candidate never
  becomes the client's error — that is always the last real dispatch failure.
  Integrity is warning-only — deleting a route/connection never mutates
  the policy; the delete RPCs surface dangling refs as warnings.
- **Seeding**: `openai-server seed-connections [--file <path>]` (or
  `ROUTER_CONNECTIONS_SEED_FILE`) provisions connections + routes from JSON
  through the same service-layer validation, requiring `ROUTER_DATABASE_URL` and
  `ROUTER_SECRETS_KEK`. **Insert-if-absent by name / public_model_id — not an
  upsert:** an existing entry is skipped, never updated, so a changed field (key
  rotation, new endpoint/prefix) has no effect. The seeder logs a warning naming
  the diverged fields; applying the change needs the UI / update API.

## 8. Encryption at rest

Provider API keys must be replayed verbatim upstream, so they cannot be hashed —
they are encrypted with **AES-256-GCM** under a KEK from `ROUTER_SECRETS_KEK`
(base64 32 bytes; fail-fast at boot when a DB is configured). Each secret is stored
as a self-describing **versioned envelope** in the single `api_key_envelope BYTEA`
column: `[format_version:1B][nonce:12B][ciphertext + GCM tag]` (`src/secrets.rs`).
The version byte is framing only (**not** fed to the AEAD as AAD), which is what
let the one-time credential backfill repack the old `(nonce, ciphertext)` columns
into the envelope with no re-encryption. `decrypt` validates the version and
minimum length before handing bytes to AES-GCM; an undecryptable row becomes the
`Unreadable` state rather than a silent fallback. Losing the KEK does not lose the
keys irrecoverably — they can be re-entered via the UI.

## 9. Metering, budgets, rate limits

- **Usage ledger** (`src/usage_tracker.rs`): each completion records a
  durable `UsageRecord` with a stable event id, completion time, resolved
  `connection_id` + upstream model id, `key_hash`, optional `usage_pool_id`,
  normalized token counts, and reported/estimated provenance. Each finalized
  call is inserted immediately with bounded retries and `ON CONFLICT
  (environment_id, event_id) DO NOTHING`, making ambiguous retries idempotent.
  Reads aggregate the durable ledger on the fly; there is no 60-second process
  buffer. **Usage pools** (`chalk_router.usage_pools`, formerly "teams") group
  keys for shared budgets and rate limits.
- **Budgets** (`src/budget.rs`, `chalk_router.budget_limits`): scope
  `usage_pool|api_key`. The supported public policies are UTC calendar token
  budgets with daily, weekly, or monthly periods; one policy may exist per
  scope, owner, and period. The definition store is separate from enforcement.
  Budgets compile into fixed-window quota reservations once per client request
  and reconcile from normalized actual usage. Rolling and cost budgets remain
  unsupported; `usage_events` is analytics data and is never queried for
  admission.
- **Rate limits** (`src/ratelimit.rs`, `chalk_router.rate_limits`): policies target
  an API key or usage pool and may be global (`connection_id = NULL`) or narrowed
  to an immutable provider `connection_id` plus zero or more **public** model
  ids. An empty model list means every model on that connection, and a non-empty
  list shares one bucket. Only `api_key|usage_pool` scopes are supported; there
  is no environment-wide quota scope. Equivalent definitions are rejected by a
  unique identity over owner, limit type, connection, and normalized model set.
  API-key creation accepts the same selector and expands configured RPM, TPM,
  and concurrency dimensions into ordinary independently manageable policy rows
  in the same transaction as the key.
  - RPM and TPM are continuously refilled **token buckets**, not leaky buckets.
    `limit_value` is the per-minute refill rate and `burst_value` is the maximum
    immediately spendable balance. TPM admission reserves the conservative
    prompt plus requested/model maximum output before dispatch, then refunds the
    difference from normalized provider-reported usage. A failed dispatched
    candidate reconciles to its prompt estimate; an incomplete stream keeps the
    conservative reservation.
    Token bucket is the LLM-oriented tradeoff: it permits an explicit short
    burst for interactive traffic, charges variable-size requests, supports a
    reservation/refund lifecycle, and updates one O(1) balance. A leaky bucket
    is attractive when the goal is a smooth departure rate, but the router
    cannot usefully queue long generations and adding queueing would inflate
    tail latency; rejecting at the leak rate also discards the desired burst.
    Fixed windows are cheaper but double-burst at boundaries, while exact
    sliding windows require per-request history. Concurrency therefore remains
    a separate lease counter rather than being approximated with either rate
    algorithm.
  - Concurrency uses one O(1) `active_count` per policy plus expiring lease rows.
    Admission increments the count and inserts a 90-second lease atomically;
    live streams renew every 30 seconds, completion/drop deletes the lease and
    decrements the count, and a later admission reaps expired leases after a
    crashed replica.
  - Admission happens after permission, cache, judge, and capability decisions,
    immediately before every primary or fallback dispatch. A limited candidate
    is a pre-dispatch skip: it never contacts that upstream, and it cannot replace
    the last genuine dispatch error if the fallback chain ultimately fails.
    A terminal local rejection is HTTP 429 with `Retry-After` and
    `x-chalk-ratelimit-*` metadata.
  - `QuotaPolicyResolver` matches definitions and produces backend-neutral
    actions. `QuotaBackend` atomically acquires all actions without loading the
    source tables. PostgreSQL locks runtime state in stable policy-ID order;
    backend errors log and fail open, while actual exhaustion returns HTTP 429.
  - Throughput is contention-shaped, not governed by one global RPS cliff.
    Independent key/connection buckets spread across Postgres locks and scale
    with the pool and database. A shared usage-pool policy is a
    hot serialized row, so its upper bound is approximately
    `1 / locked-transaction-latency` (for example, 2 ms admits roughly 500 RPS;
    5 ms roughly 200 RPS for that one bucket). Deployments expecting sustained
    higher rates on one shared bucket should load-test their database and shard
    the policy or move to a purpose-built distributed limiter. Pool wait and
    admission latency must be monitored because database degradation eventually
    invokes the fixed fail-open behavior.

  `postgres` (durable) and `memory` (volatile; dev/test only) share enforcement
  contract tests. `redis` is a reserved setting until its atomic implementation
  passes the same suite. See RFP-0007 for the schema and lifecycle contract.

## 10. Observability / tracing

OTel is initialized at boot (`src/telemetry.rs`) with an OTLP **gRPC** exporter
(HTTP/2 keepalive to survive LB idle-drops, optional TLS), env-driven sampler, and
W3C context propagation. Inbound `traceparent`/`tracestate` are extracted so the
router **continues the caller's trace** (with a workaround for opentelemetry-rust's
strict traceparent parser).

All LLM spans conform to **OpenInference** conventions (see `AGENTS.md`):
`openinference.span.kind`, `llm.system`/`llm.provider`, `llm.model_name`,
`llm.invocation_parameters`, `llm.input_messages.*`/`llm.output_messages.*`
(roles always; content gated), `llm.token_count.{prompt,completion,total}`,
`input.value`/`output.value`, plus `embedding.*` for embeddings and object-store
links (`audio.0.url`, `image.N.url`). The `set_route_span_attributes` helper stamps
`llm.system`/`llm.model_name`/`chalk.connection_{id,name}` from the resolved
`RouteResolution` **before** the permission gate, so even denied/errored spans
carry the connection identity; a winning fallback re-stamps from its own resolution.

`POST /v1/traces/spans` (`src/handlers/traces.rs`) ingests already-finished client
spans (session/turn/tool/agent/chain/llm), validates W3C ids, maps kinds to
OpenInference, and re-emits them through the router's OTLP pipeline — stitching a
client's agent spans with the router's LLM spans into one trace.

**Content-logging gate**: `ROUTER_TRACE_LOG_CONTENT` (default **on**) controls whether
prompt/response bodies (`input.value`/`output.value`, message content, tool-call
arguments, embedding text) land on spans; when off, only metadata (model, token
counts, roles, latency) is recorded.

## 11. Storage & migrations

Every durable record lives in the `chalk_router` Postgres schema. Stores are trait
objects selected once at boot: **all Postgres** (when `ROUTER_DATABASE_URL` is set)
or **all in-memory** (local dev; in-memory stores are intentionally single-tenant).
The app **never creates tables at boot** — migrations run first via `openai-server
migrate` (transaction-pool-safe renewable lease; schema-qualified DDL), typically
as a k8s Job before the Deployment rolls. `ROUTER_USAGE_BACKEND` selects the
analytics ledger independently. Budget and rate-limit definitions follow the
core store, while `ROUTER_QUOTA_BACKEND` selects their shared mutable admission
state (`postgres` durable, `memory` volatile, `disabled`, or the reserved
`redis` value).

## 12. Configuration (selected env vars)

| Var | Purpose |
|---|---|
| `ROUTER_DATABASE_URL` | Core Postgres pool; unset ⇒ in-memory dev stores |
| `CHALK_ENVIRONMENT_ID` | Tenant boundary; **required** when a DB is set |
| `ROUTER_KEY_PEPPER` | HMAC pepper for API-key hashing; required on the DB path |
| `ROUTER_SECRETS_KEK` | base64 32-byte KEK for credential encryption; required on the DB path |
| `ROUTER_USAGE_BACKEND` | Usage-ledger backend (`postgres` implemented) |
| `ROUTER_QUOTA_BACKEND` | Shared budget/rate-limit state (`disabled|memory|postgres`; `redis` reserved) |
| `ROUTER_ADMIN_KEY`, `CHALK_JWT_ISSUERS`, `CHALK_JWT_AUDIENCE`, `ROUTER_ALLOW_UNAUTHENTICATED`, `ROUTER_ACCEPT_CROSS_ENVIRONMENT_TOKENS` | Auth |
| `ROUTER_CONNECTIONS_SEED_FILE` | Default seed file for `seed-connections` |
| `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `VOYAGE_API_KEY` | **Seed-only**, interpolated into the seed template as `${…}`; not read by the serving process |
| `ROUTER_OBJECT_STORE_URI` (+ cloud creds) | Object storage for span media uploads |
| `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_TRACES_SAMPLER[_ARG]`, `ROUTER_TRACE_LOG_CONTENT` | Tracing |
| `PORT` (default 4001), `RUST_LOG` | Server |

## 13. Deployment

The self-managed chart in `charts/chalk-router/` deploys the API, UI, and reverse
proxy using operator-provided images and credentials. A migration Job runs before
rollout. See [self-managed setup](../docs/self-managed-setup.md). Deployment pipelines
and provider seeding are operator-owned; no company infrastructure is required.

## 14. Implemented vs deferred

**Implemented:** the proxy data plane, including Chat Completions, Responses,
Anthropic Messages/count-tokens, embeddings, images, and audio; provider
connections + model routes + routing (§7); encryption at rest (§8); hashed API
keys, permission allowlists (chat/Responses/Anthropic/embeddings/images), usage
pools, UTC calendar token budgets, and RPM/TPM/concurrency rate-limit
enforcement (§5, §9); capability profiles and protocol-aware dispatch
(RFP-0004/RFP-0005); durable per-call PostgreSQL usage accounting;
backend-neutral memory/PostgreSQL quota state (RFP-0007); OpenInference tracing
and span ingest (§10); the two-wire admin plane + UI; `environment_id` tenancy;
`migrate`/`seed-connections`.

**Deferred / not yet built** (each has its own RFP or is a known stub):
- **Mid-stream failover** — streaming fallback covers the window before the `200`
  is committed ([RFP-0002](0002-streaming-fallback.md), built); restarting a stream
  on another provider once bytes are flowing is an explicit non-goal.
- **Stall bound for non-streaming dispatch** — the per-attempt deadline is
  streaming-only, since a non-streaming call spans the full generation
  ([RFP-0002 §5.4](0002-streaming-fallback.md)).
- **Audio permission scoping** — explicitly deferred; moderation remains a local
  stub ([RFP-0003](0003-permission-scope-non-llm-endpoints.md)).
- **ClickHouse usage backend** — selectable but stubbed (§11).
- **Redis quota backend** — the config value and backend contract are reserved,
  but no atomic Redis implementation exists (RFP-0007).
- **Per-request cost tracking** — no cost on the ledger, so `cost_micro_usd` budgets and `soft_ratio` warns are inert (§9).
- **Stricter federated-env isolation** — reject claimless cross-env JWTs (§6).

## 15. Key design decisions (rationale)

- **Hash client keys, encrypt provider keys.** Client keys are verified, so a
  one-way HMAC suffices (and enables O(1) lookup). Provider keys must be replayed
  verbatim, so they're reversibly encrypted (§8) — never hashed.
- **Multiple connections per provider kind.** Names and non-null prefixes remain
  unique per environment. Canonical-prefix ownership selects the default for bare
  model ids; otherwise a sole eligible connection is accepted and ambiguity is a
  configuration error. `explicit` connections remain route-only.
- **Resolve the route exactly once.** Carrying a single `ResolvedRoute` through
  permission → dispatch → attribution means a mutable 5s snapshot can't shift a
  request between connections mid-flight (a TOCTOU class of bug), and attribution
  never re-resolves.
- **SWR + single-flight everywhere.** Resolution and discovery both serve stale
  instantly and refresh once in the background, so steady-state routing is DB-free,
  survives provider/DB blips, and never stampedes an upstream.
- **Two wire formats, one service layer.** HTTP (back-compat) and gRPC/gRPC-Web
  (UI) are thin adapters over `src/service/`, so semantics can't drift by transport.
- **Warning-only integrity.** Connections, routes, and the fallback JSON are not
  bound by DB foreign keys; dangling refs surface as diagnostics/warnings rather
  than blocking deletes or requiring cross-store transactions.
- **Pluggable metering backends.** Usage/budget/rate-limit are traits selected by
  env, so a plain-Postgres deployment works today and a ClickHouse backend can
  drop in without touching the request path.
