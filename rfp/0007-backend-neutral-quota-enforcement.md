# RFP-0007: Backend-Neutral Quota Enforcement

- **Status:** Accepted / Implemented
- **Created:** 2026-09-02
- **Revised:** 2026-09-03 — PR #173 landed; UTC daily/weekly/monthly budgets,
  shared usage-pool management, and unique policy identities followed
- **Depends on:** [RFP-0001](0001-router-motivation-and-architecture.md) — API keys, usage pools, rate limits, and budgets
- **Related:** [RFP-0006](0006-usage-attribution-and-cost-accounting.md) — usage events remain the analytics ledger, not the admission-control store
- **Implemented by:** [PR #173](https://github.com/chalk-ai/chalk-router/pull/173),
  with subsequent calendar-budget and policy-management follow-ups
- **Scope:** Enforce existing token rate limits and token budgets through one internal quota interface, using memory or Postgres now and permitting Redis later without moving policy definitions out of Postgres.
- **Non-goals:** Replacing or merging `rate_limits` and `budget_limits`; monetary cost; environment-wide quotas; rolling-window budgets; arbitrary metadata/group-by rules; billing; a Redis implementation; or deriving admission decisions from `usage_events`.

---

## 0. Summary

Rate limits and usage budgets are different product concepts but the same
runtime problem: atomically decide whether a request may consume a scarce unit,
reserve enough capacity before work starts, and reconcile the reservation when
the result is known.

This RFP keeps their control-plane representations separate:

- `rate_limits` describes short-horizon request, token, and concurrency limits
  for an API key or usage pool. A rate limit is global by default and may be
  narrowed to one connection and an optional model set.
- `budget_limits` continues to describe long-horizon token ceilings for an API
  key or usage pool.

It introduces a deliberately small internal quota layer:

1. `rate_limits` owns its complete public definition, including optional
   connection/model applicability and burst capacity.
2. `quota_policies` is a pure, checked adapter to exactly one rate-limit or
   budget source. It gives every enforceable policy a stable global identity.
3. `QuotaPolicyResolver` reads, matches, and compiles control-plane definitions.
4. `QuotaBackend` atomically changes runtime state for already-compiled
   policies. Its contract does not require the backend to query Postgres.
5. Memory and Postgres implement the same contract now. Redis can implement it
   later using the same compiled actions and stable policy IDs.
6. `usage_events` remains an asynchronous analytics and audit ledger. It is not
   read to admit a request and is not required for correct quota enforcement.

The immediate correctness win is budget enforcement. When enabled, the current
Postgres `BudgetBackend::check` sums asynchronously flushed `usage_events`;
RFP-0001 documents up to one 60-second flush interval of overshoot, and the
check cannot atomically reserve capacity across requests. Moving budgets to
pre-request reservation removes both races. The ledger remains useful for
reporting.

The public admin surfaces remain “rate limits” and “budgets.” “Quota” is an
internal execution term, not a third product abstraction.

## 1. Constraints and decisions

Rate limits and budgets both remain useful. A rate limit controls **velocity**:
it protects upstream capacity and smooths bursts over seconds or minutes. A
budget controls **volume**: it caps cumulative consumption over a calendar
period. A client can be below its daily budget while sending an unsafe burst,
or can send slowly enough to satisfy RPM while eventually exhausting its
budget. Sharing an atomic reservation engine does not make the policies
interchangeable.

### 1.1 Preserve the source concepts and tenant boundaries

During implementation, PR #173 replaced its then-unmerged migration and code,
but did not merge the product concepts represented by `rate_limits` and
`budget_limits`.
It evolves `rate_limits` into one complete definition table and makes one
tenant-key correction to `budget_limits`: the latter changes from `PRIMARY KEY (id)` to `PRIMARY KEY
(environment_id, id)`, matching the tenant-scoped identity used by
`rate_limits` and the rest of the core schema. In particular:

- Do not merge the source tables.
- Do not move budget rows into `rate_limits` or rate-limit rows into
  `budget_limits`.
- Add `burst_value`, nullable `connection_id`, and `models` directly to
  `rate_limits`; remove the redundant `target_type` and `target_value` columns.
- Existing ID values do not change. Rate-limit and budget definitions are
  identified relationally by `(environment_id, id)`; admin APIs continue to
  derive `environment_id` from the authenticated environment rather than ask
  callers to supply it. The bridge ID is the stable internal runtime identity.

The owning service must create, update, or delete a source and its bridge row in
one transaction. A source row without a bridge is visible as not enforceable;
it must never be silently presented as an active policy.

### 1.2 Supported policies

The first implementation supports:

| Source | Algorithm | Meter | Window |
| --- | --- | --- | --- |
| requests per minute | token bucket | requests | continuous refill |
| tokens per minute | token bucket | tokens | continuous refill |
| concurrent requests | renewable lease | active requests | lease lifetime |
| calendar token budget | fixed window | tokens | source-defined calendar window |

The current public calendar periods are UTC daily, weekly, and monthly windows.
At most one token budget may exist for the same environment, scope, owner, and
period.

Only `api_key` and `usage_pool` scopes are valid. The external UI may call an
API key a token, but the compiled type uses `ApiKey` to avoid confusing the
credential with token usage.

`environment_id` is still required as the tenancy and keyspace boundary. It is
not a quota scope: there is no policy meaning “limit the whole environment.”
Every key-scoped or pool-scoped policy is nevertheless owned by exactly one
environment.

The migration normalizes the historical rate-limit scope spelling `token` to
`api_key`. Both source tables thereafter use `api_key` and `usage_pool`.

The following definitions are explicitly unsupported in this RFP:

- environment-wide limits;
- `cost_micro_usd` budgets; and
- rolling-window budgets.

Unsupported rows fail validation and are not partially or approximately
enforced. In particular, a rolling budget is not converted into a token bucket:
those semantics differ at the window boundary.

### 1.3 One runtime backend

One process setting chooses the mutable-state implementation:

```text
ROUTER_QUOTA_BACKEND=disabled|memory|postgres|redis
```

`redis` is reserved until implemented. Policy definitions remain available
through the policy store regardless of runtime backend. An enabled policy must
not be accepted by the admin service when enforcement is disabled or its source
definition cannot be compiled.

Definition persistence follows the router's core store: Postgres when the core
database is configured and memory in database-less development. It is not
selected by `ROUTER_QUOTA_BACKEND`; that setting controls mutable admission
state only.

This replaces `ROUTER_BUDGET_BACKEND` and `ROUTER_RATELIMIT_BACKEND` for
enforcement. The never-implemented `chalk_service` rate-limit value is dropped;
a remote quota service can be proposed later as a real `QuotaBackend`.
`ROUTER_USAGE_BACKEND` is unchanged and continues to select the analytics
ledger independently. This does not merge the rate-limit and budget admin APIs.

### 1.4 Explicit operation applicability

Authentication is not a quota admission point. Token reservations require the
parsed request and its resolved route, so restoring the old middleware query
would reintroduce the usage-ledger race this RFP removes. Every public operation
instead belongs to one explicit class, and covered handlers use the shared
request/dispatch admission helpers:

| Class | Operations | Applied policies |
| --- | --- | --- |
| token-metered | chat completions, Responses create and compact, Anthropic Messages, embeddings | budgets, global RPM/TPM/concurrency, and matching dispatch RPM/TPM/concurrency |
| request-only | cache hits, image and audio generation, Anthropic token counting, stored Responses retrieval/deletion | global and, when a route is resolved, matching dispatch RPM/concurrency; token policies are inapplicable |
| exempt | health, admin/control-plane, traces, model discovery, and local stubs | none |

`unknown` and `not applicable` are different token states. A token-metered
operation without a conservative estimate is rejected only if a matching token
policy exists. A request-only operation omits token policies and therefore does
not let a token budget accidentally disable image, audio, or cached responses.
Cache hits consume request velocity but no token capacity because they do not
buy provider tokens or write usage events.

Unknown token cost is an admission requirement, not a zero-sized quota action.
The resolver may still compile matching request or concurrency actions, but it
must reject before sending any action batch to the quota backend when a matched
budget or TPM definition requires a conservative token reservation. That
rejection is a 400, not a 429: retrying cannot produce an estimate.

Provider-hosted tools (Responses `web_search*`, `file_search`,
`code_interpreter`, `mcp`, `image_generation`; Anthropic `web_search_*`,
`web_fetch_*`, `code_execution_*` and MCP servers) are a soft cap, not an
unknown estimate. The provider adds their output to the model context without
it crossing the router, so no admission-time bound exists. Each matching token
policy holds the visible bound plus a fixed hosted-tool reserve, clamped to the
policy's limit or burst so the reserve alone never rejects. Settlement charges
actual usage, and anything beyond the reservation is debt. Per-request
overshoot is therefore unbounded; the reserve limits how many hosted requests
are admitted together, and debt stops later ones. An unknown outcome keeps the
reservation, and no missing-usage estimate is derived from the visible input.
Caller-run tools (function, custom, computer use, local shell, apply patch)
return results on the wire and stay bounded. Responses tool types that are not
classified remain unknown.

The same soft cap applies to the other input a provider reads that the wire
does not bound, on Chat, Responses (including compaction) and Messages:
references to provider-stored history (`previous_response_id`, `conversation`,
`item_reference` items), and opaque content whose token cost only the provider
knows (image, audio, file and document parts, and `encrypted_content`
reasoning state). The visible bound counts the request bytes outside the
opaque subtrees; references count as their own bytes. Gemini interactions,
Decisions and judge calls keep rejecting such input as unknown. A request
without an output bound (no `max_tokens` and no known model maximum) also
remains unknown.

## 2. Database schema

### 2.1 Rate-limit definitions

Connection and model selection belongs to the rate limit. It does not belong to
the generic quota adapter, and it does not belong to a token budget: budgets in
this RFP apply to their API key or usage pool regardless of which route serves
the request. The existing table becomes the complete definition:

```sql
ALTER TABLE chalk_router.rate_limits
    ADD COLUMN connection_id TEXT,
    ADD COLUMN models TEXT[] NOT NULL DEFAULT '{}',
    ADD COLUMN burst_value BIGINT,
    DROP COLUMN target_type,
    DROP COLUMN target_value,
    ADD CONSTRAINT rate_limits_connection_fk
        FOREIGN KEY (environment_id, connection_id)
        REFERENCES chalk_router.provider_connections (environment_id, id)
        ON DELETE RESTRICT,
    ADD CONSTRAINT rate_limits_models_require_connection_check
        CHECK (connection_id IS NOT NULL OR cardinality(models) = 0),
    ADD CONSTRAINT rate_limits_burst_check
        CHECK (burst_value IS NULL OR burst_value > 0),
    ADD CONSTRAINT rate_limits_concurrency_burst_check
        CHECK (limit_type <> 'concurrent_requests' OR burst_value IS NULL);

CREATE INDEX idx_router_rate_limits_active_selector
    ON chalk_router.rate_limits
        (environment_id, scope, scope_id, connection_id)
    WHERE enabled;
```

`connection_id IS NULL` and `models = '{}'` means all traffic for the key or
pool and is acquired once per client request. A connection with an empty model
array means every model on that connection; a non-empty array narrows the
selector. Models without a connection are invalid. Model values are normalized
with the same model-ID rules used by routing before they are stored. For RPM and
TPM, a null `burst_value` means the one-minute limit is also the bucket capacity.
Concurrency policies leave it null because `limit_value` is already the
active-request cap.

Deleting a referenced connection is restricted until its rate limits are
deleted. The migration backfills exact legacy connection targets, normalizes
the API-key scope spelling, and aborts rather than guessing if it finds an
unsupported target, scope, type, value, or missing connection.
The partial selector index keeps admission lookup proportional to the matching
key and pool policies instead of all rate-limit definitions.

The current schema additionally canonicalizes each model selector as a sorted,
deduplicated set and permits only one equivalent rate-limit definition per
environment, owner, limit type, connection, and model set. Global and
connection-scoped policies use separate partial unique indexes; the former
rate-limit `name` column has been removed.

### 2.2 Policy bridge

Before creating the bridge, a forward migration makes the existing budget
identity tenant-scoped:

```sql
ALTER TABLE chalk_router.budget_limits
    DROP CONSTRAINT budget_limits_pkey,
    ADD CONSTRAINT budget_limits_pkey
        PRIMARY KEY (environment_id, id);
```

This is a migration of the existing table, not an edit to the already-applied
`0001_router_core.sql`. The old global primary key guarantees that existing
rows cannot collide under the new composite key, and existing ID values remain
unchanged. There are no pre-existing foreign keys to migrate; the quota bridge
below is the first. Deployments should still perform the usual preflight check
for unexpected dependent objects before taking the table lock required by the
primary-key replacement.

```sql
CREATE TABLE chalk_router.quota_policies (
    id                TEXT PRIMARY KEY,
    environment_id    TEXT NOT NULL,

    -- Exactly one source arm is populated.
    rate_limit_id     TEXT,
    budget_limit_id   TEXT,

    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT quota_policy_source_check CHECK (
        (
            rate_limit_id IS NOT NULL
            AND budget_limit_id IS NULL
        )
        OR
        (
            rate_limit_id IS NULL
            AND budget_limit_id IS NOT NULL
        )
    ),

    CONSTRAINT quota_policy_rate_limit_unique
        UNIQUE (environment_id, rate_limit_id),
    CONSTRAINT quota_policy_budget_limit_unique
        UNIQUE (environment_id, budget_limit_id),

    CONSTRAINT quota_policy_rate_limit_fk
        FOREIGN KEY (environment_id, rate_limit_id)
        REFERENCES chalk_router.rate_limits (environment_id, id)
        ON DELETE CASCADE,
    CONSTRAINT quota_policy_budget_limit_fk
        FOREIGN KEY (environment_id, budget_limit_id)
        REFERENCES chalk_router.budget_limits (environment_id, id)
        ON DELETE CASCADE
);

```

`environment_id` belongs to every quota policy, not only rate limits. The prior
name `rate_limit_environment_id` existed only to make the nullable composite
rate-limit foreign key convenient; it encoded the foreign-key mechanics in the
column name and obscured the policy's actual ownership. A generic non-null
column also becomes the natural Redis hash-tag input.

The check guarantees exactly one source. Both arms reference their source table
through the same environment-qualified identity. Postgres therefore rejects a
bridge whose `environment_id` differs from its source definition's environment;
tenant consistency does not depend on a service-layer validation check. The
unique constraints make the bridge one-to-one in both directions.

### 2.3 Runtime state

```sql
CREATE TABLE chalk_router.quota_states (
    policy_id       TEXT PRIMARY KEY
                    REFERENCES chalk_router.quota_policies (id)
                    ON DELETE CASCADE,

    -- Token-bucket state.
    balance         NUMERIC,
    updated_at      TIMESTAMPTZ,

    -- Fixed-window state.
    window_start    TIMESTAMPTZ,
    used_amount     BIGINT NOT NULL DEFAULT 0 CHECK (used_amount >= 0),

    -- Concurrency state. Expired leases are removed before this is read.
    active_count    BIGINT NOT NULL DEFAULT 0 CHECK (active_count >= 0)
);

CREATE TABLE chalk_router.quota_reservations (
    policy_id       TEXT NOT NULL
                    REFERENCES chalk_router.quota_policies (id)
                    ON DELETE CASCADE,
    permit_id       UUID NOT NULL,
    reserved_amount BIGINT NOT NULL CHECK (reserved_amount >= 0),
    window_start    TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at      TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (policy_id, permit_id)
);

CREATE INDEX quota_reservations_permit_idx
    ON chalk_router.quota_reservations (permit_id);
CREATE INDEX quota_reservations_expiry_idx
    ON chalk_router.quota_reservations (expires_at);

CREATE TABLE chalk_router.quota_leases (
    policy_id       TEXT NOT NULL
                    REFERENCES chalk_router.quota_policies (id)
                    ON DELETE CASCADE,
    permit_id       UUID NOT NULL,
    expires_at      TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (policy_id, permit_id)
);

CREATE INDEX quota_leases_permit_idx
    ON chalk_router.quota_leases (permit_id);
CREATE INDEX quota_leases_expiry_idx
    ON chalk_router.quota_leases (policy_id, expires_at);
```

One sparse state table is preferred to three algorithm-specific state tables.
The algorithm determines which fields are meaningful. The corresponding Rust
code must validate that unused state cannot influence a decision.

In database-less development and tests, the memory policy store assigns the
same stable IDs for its lifetime and enforces the same one-of-source validation
in Rust. It does not emulate SQL tables.

No usage-event backfill is needed. There is no enforced runtime state to
migrate, and historical analytics rows are not a safe substitute for an atomic
admission counter.

Reservations expire after the fixed window ends or after a bounded maximum
request lifetime. Expiry deletes the reconciliation record but does not refund
the conservative charge. This bounds table growth without letting an abandoned
request restore capacity it may have consumed.

Fixed-window quota state is admission-window enforcement state, not an exact
historical usage ledger. Acquisition immediately charges the conservative
reservation to the calendar window containing admission. Settlement reconciles
that charge only while the same window remains current; after rollover, it
removes the old reservation without changing the new window's `used_amount`.
Consequently, a request that completes after its admission window ends cannot
refund unused tokens or carry excess usage into the next window. The separate
`usage_events` ledger remains the historical record of actual usage.

Lazy cleanup on acquisition is not sufficient for a policy that goes cold. A
small periodic janitor deletes expired reservations and, while holding the
corresponding state-row lock, deletes expired leases and decrements
`active_count`. It works in bounded batches and uses the expiry indexes above.
Postgres implements the janitor now; a future Redis backend uses key expiry for
reservations and leases plus an atomic repair path for concurrency state.

### 2.4 Background Responses ownership

Background Responses attach nullable `(background_environment_id,
background_event_id)` ownership directly to token reservations and concurrency
leases. Composite foreign keys reference the durable accounting job, and partial
indexes support lookup by job. There is no separate grants ledger. Ordinary
foreground entries retain their existing expiry and Drop behavior.

Owned token reservations additionally capture the original algorithm, limit,
burst, and failed-attempt amount. Their existing reserved amount and window
boundary complete the settlement snapshot; no compiled action arrays or current
policy-definition reads are needed to recover a job after restart.

The shared admission function validates the submission intent and attaches
ownership atomically with reserving capacity. Both admission-time expiry and
bounded cleanup exclude owned rows. The background reconciler finds all owned
permits and invokes the shared settlement function, locking the job before policy
states in stable policy-ID order. Usage insertion, reservation reconciliation,
lease release, and the job's settled marker commit in one Postgres transaction.
Deleting settled reservation/lease rows makes replayed settlement a no-op.

Confirmed terminal responses without usable usage keep their conservative token
charge but release concurrency. Ambiguous submissions retain both until their
execution can be resolved; HTTP completion, client disconnects, and router
outages are not evidence that provider execution has stopped. Restart-safe
enforcement requires Postgres accounting and Postgres quotas; the memory backend
implements the same transitions but remains per-process and volatile.

## 3. Compiled policy contract

The policy resolver converts either source row into a common value before
calling the runtime backend:

```rust
struct QuotaPolicy {
    id: String,
    environment_id: String,
    source: QuotaSource,
    algorithm: QuotaAlgorithm,
    meter: QuotaMeter,
    limit: u64,
    burst: Option<u64>,
    window: Option<FixedWindow>,
    scope: QuotaScope,
    selector: Option<RateSelector>,
}

enum QuotaSource {
    RateLimit { id: String },
    BudgetLimit { id: String },
}

enum QuotaAlgorithm { TokenBucket, FixedWindow, Concurrency }
enum QuotaMeter { Requests, Tokens, ConcurrentRequests }
enum QuotaScope { ApiKey(String), UsagePool(String) }

struct RateSelector {
    connection_id: String,
    models: Vec<String>, // empty means all models on the connection
}
```

A missing selector is not an override marker: it means the rate limit applies
globally and is acquired at request admission. A present selector is matched
and acquired at dispatch admission. Models cannot be present without a
connection.

Dispatch matching represents the model state explicitly. An exact public model
matches connection-wide policies plus policies whose `models` contain that
model. An operation with no model dimension matches only connection-wide
policies. If the request delegates model selection to the provider, as image
generation may do when `model` is omitted, connection-wide policies remain
eligible; however, the router rejects the request with HTTP 400 and requires an
explicit model when any otherwise-applicable model-scoped policy exists. This
fails closed without inventing a provider default or charging every model
bucket. A token-per-minute policy is not otherwise applicable to an operation
that has no token accounting and therefore does not by itself require a model.

Calendar boundaries, including timezone and anchor behavior, are computed by
the policy resolver. The backend receives an exact `window_start` and
`window_end`; it does not implement calendar arithmetic.

The policy resolver may cache compiled policies briefly. Cache invalidation is a
control-plane freshness concern, not part of runtime counter correctness. The
admin write path invalidates local cache entries after commit.

## 4. Runtime backend contract

```rust
#[async_trait]
trait QuotaBackend {
    async fn acquire_many(
        &self,
        actions: &[QuotaAction],
    ) -> Result<QuotaDecision, QuotaBackendError>;

    async fn finish(
        &self,
        permit: &QuotaPermit,
        outcome: QuotaOutcome,
    ) -> Result<(), QuotaBackendError>;

    async fn renew(
        &self,
        permit: &QuotaPermit,
    ) -> Result<(), QuotaBackendError>;
}
```

`QuotaAction` is a fully matched, compiled policy plus the requested reservation
amount and any fixed-window boundary. The backend never needs to load
`rate_limits`, `budget_limits`, API keys, usage pools, connections, or models.

`acquire_many` has one essential guarantee: all applicable actions succeed or a
rejected request consumes capacity from none of them and receives no permit.
Refill, window-reset, and expired-record cleanup may still advance bookkeeping.
Policies are processed in stable ID order in Postgres to avoid deadlocks.

The Postgres implementation uses one stored function that accepts the compiled
actions, creates missing state rows with `INSERT ... ON CONFLICT DO NOTHING`,
locks the relevant `quota_states` rows in stable policy-ID order, deletes
expired leases, applies refills/window resets, checks every action, and commits
reservations only when all checks pass. The stored function must not query
policy-definition tables.

A future Redis implementation receives the same actions and executes the same
transition atomically in Lua. Definitions may stay in Postgres because Redis
stores only state, reservations, and leases keyed by `quota_policy.id`. If Redis
Cluster is used, all keys for one acquisition must share an environment hash
tag so the script remains single-slot. This preserves atomic multi-policy
admission, but it also places all quota traffic for one environment on one
Redis Cluster slot and therefore one shard. A single very hot environment can
become a shard-level scaling ceiling. The first Redis implementation must either
capacity-plan for that constraint or use a non-clustered Redis deployment; it
must not silently split one acquisition across slots and weaken atomicity.

Memory, Postgres, and future Redis backends must pass the same conformance test
suite. Backend failures fail open, emit an error metric and structured log, and
never masquerade as a quota rejection. An actual quota rejection returns HTTP
429 and identifies whether the blocking source was a rate limit or budget.
`finish` and `renew` are idempotent by permit ID. A finish after reservation
expiry is a no-op because the conservative charge has already been retained.

The trait is intentionally additive. A later GCRA or sliding-window algorithm
adds an enum variant and backend transition without changing request
orchestration, scope resolution, permit identity, or finish/renew idempotency.

## 5. Request lifecycle

Rate limits and budgets run through the same backend at one of two points:

| Point | Policies | Reason |
| --- | --- | --- |
| request admission | API-key and usage-pool budgets plus global rate limits | Charge once for the client request, independent of fallback attempts |
| dispatch admission | matching connection/model rate limits | Each upstream attempt consumes provider-facing capacity |

Every matching policy is cumulative. Direct API-key and inherited usage-pool
policies both apply; a connection/model rate limit does not replace a broader
connection limit. Specificity determines whether a policy matches, never
whether it overrides another policy:

```text
request admission = key budgets
                  AND pool budgets
                  AND global key rate limits
                  AND global pool rate limits

dispatch admission = connection-wide rate limits
                   AND connection/model rate limits
```

All actions within one admission point are acquired atomically. If any action
rejects, none of that batch consumes capacity. When several actions reject, the
backend reports the one with the longest retry interval, using stable policy ID
order to break ties. Request and dispatch acquisition cannot be one transaction
because route selection and fallback occur between them.

The router performs these steps:

1. Authenticate the API key and resolve its optional usage pool.
2. Resolve and validate the primary plus every configured fallback exactly
   once. Keep the viable routes as the immutable dispatch plan.
3. Compile matching calendar token budgets and global rate limits, then acquire
   them together once. A client-supplied output cap applies to every candidate;
   otherwise token policies reserve estimated input plus the largest declared
   maximum output across all viable routes. If any viable route has no declared
   ceiling, the reservation is unknown.
4. Before each primary or fallback dispatch, compile matching rate limits for
   the selected connection and model, then acquire them atomically. A
   provider-selected model is admitted only when no applicable model-scoped
   definition makes the match ambiguous; otherwise require the client to send
   an explicit public model before dispatch.
5. Renew concurrency leases and foreground token reservations while a stream or
   long request remains alive, so settlement always finds the reservation.
6. Settle token reservations with normalized actual usage when the response is
   complete. A known non-clean stream end explicitly settles with unknown usage,
   retaining the conservative token charge while releasing concurrency; permit
   drop and lease expiry remain crash/disconnect fallbacks.

If a request is abandoned before any upstream dispatch, its budget and global
TPM reservations are refunded and concurrency leases are released. An admitted
global RPM unit remains consumed because the router accepted the client
request. Once dispatched, an incomplete response with unknown actual usage
keeps the conservative token reservation. A completed response refunds unused tokens.
If a provider reports more tokens than the conservative reservation, settlement
records the excess as debt and future acquisitions observe it; settlement must
never turn a previously admitted request into a late rejection.

Debt does not compound: an idempotent settlement can add at most
`actual - reserved` once for that permit. Once the state is over limit, no new
request is admitted until refill or window reset; the maximum transient debt is
therefore bounded by the under-reservation across requests that were already in
flight concurrently. A fixed-window request that finishes after its window
closes charges its excess to the next window, opening it if no admission has.

A budget and global TPM policy measure provider tokens bought for the one
client request. They do not charge cache hits or double-count fallback attempts.
Dispatch TPM measures each upstream attempt; failed attempts retain their
prompt-token reservation when actual usage is unknown. Global concurrency
covers the logical request; connection concurrency covers one dispatch. This
distinction is intentional and will need reconsideration when monetary cost is
introduced.

The request produces `usage_events` through the existing asynchronous
accounting path after enforcement. Dropping, delaying, or retrying that event
does not change quota state.

## 6. Control-plane behavior

- Rate-limit CRUD remains rate-limit CRUD. It writes `rate_limits` and the rate
  arm of `quota_policies` in one transaction.
- Budget CRUD remains budget CRUD. It writes `budget_limits` and the budget arm
  of `quota_policies` in one transaction.
- API-key and usage-pool views list their attached source rows through normal
  indexed queries. They do not scan every budget or rate-limit row and join in
  application memory.
- An API-key list loads direct budgets by key hash and inherited budgets by the
  key's `usage_pool_id`, using a batched query or SQL join. It does not fetch all
  budget rows per key, and it does not read `quota_states` merely to show policy
  configuration.
- The existing polymorphic `scope_id` columns remain a logical association, not
  a foreign key to API keys or usage pools. Services join by
  `(environment_id, scope, scope_id)` and validate the referenced key or pool
  when writing a policy. The quota bridge does not weaken or disguise that
  relationship.
- A judge policy remains authorization/configuration selected by normal
  precedence rules. It is not a quota and does not belong in these tables.
- API-key creation with inline budgets or rate limits uses one co-located
  PostgreSQL statement for the key, source definitions, and bridge rows. The
  non-Postgres fallback revokes the credential before compensating definition
  cleanup, attempts every cleanup, and surfaces any incomplete rollback; write
  errors are never discarded.
- A future per-key judge-policy override should be a nullable, foreign-keyed
  control-plane reference on the API key or a dedicated override table; it must
  not be encoded as a quota policy.
- Deleting a source row cascades its bridge and runtime state. A referenced
  connection cannot be deleted until its rate limits are deleted. Deleting
  runtime state only resets enforcement and does not delete policy definitions.

## 7. Competitive context

The LLM gateways below are useful comparisons for product semantics. Most do
not publish enough storage or atomicity detail to validate the backend boundary,
so infrastructure rate limiters are considered separately in §7.6.

### 7.1 LiteLLM

[LiteLLM documents](https://github.com/BerriAI/litellm-docs/blob/main/docs/proxy/users.md)
budgets, RPM, TPM, and parallel-request limits across keys, users, teams,
customers, and models. It also documents a budget reservation flow that
estimates maximum cost before a request and reconciles actual cost afterward.
For multi-replica deployments it uses Redis-backed counters, with an optional
database cross-check for stricter budget enforcement.

That validates two choices here: reserve before performing expensive work, and
separate durable definitions from low-latency mutable counters. LiteLLM's much
broader hierarchy and monetary accounting are intentionally outside this first
version. Chalk uses only API-key and usage-pool scopes and token units.

### 7.2 OpenRouter

[OpenRouter's API-key API](https://openrouter.ai/docs/api/api-reference/api-keys/create-keys)
supports an optional per-key USD limit with daily, weekly, or monthly reset and
reports remaining spend. Its provider and platform rate limits are presented as
a separate capacity concern.

OpenRouter's key-centric spend ceiling is easy to understand, but it is not a
general connection/model/token quota contract. Chalk likewise keeps budgets
and rate limits distinct in the product surface, while sharing only the
admission machinery underneath. Cost limits are deferred.

### 7.3 Fireworks AI and Nexus

[Fireworks account quotas](https://docs.fireworks.ai/guides/quotas_usage/account-quotas)
separate account-wide request-rate and monthly-spend controls, while
[serverless rate limits](https://docs.fireworks.ai/serverless/rate-limits) are
adaptive and expressed per account and model in token throughput. The public
[Nexus product page](https://fireworks.ai/nexus) describes account budget
defaults with per-user overrides and spend reporting by user, model, and API
key.

The useful product lesson is to keep capacity protection and spend governance
as separate concepts even if enforcement shares primitives. Default-plus-
override policy and spend reporting may be valuable later, but they would add
precedence and monetary semantics not required for PR #173. Fireworks does not
publicly specify enough of the runtime algorithm to compare storage or
reservation mechanics.

### 7.4 Portkey

[Portkey budget policies](https://portkey.ai/docs/product/enterprise-offering/budget-policies)
are the closest public analogue to a general quota engine: a policy may express
usage or rate limits, choose request/token/cost units and reset periods, apply
conditions, and group counters by dimensions such as API key, provider, model,
or metadata.

This RFP deliberately avoids that policy language. Chalk already has two
source schemas and needs reliable enforcement, not arbitrary runtime grouping.
Fixed API-key/usage-pool scopes plus an optional rate selector keep counter
cardinality, validation, and Redis atomicity understandable.

### 7.5 Helicone

[Helicone custom rate limits](https://docs.helicone.ai/features/advanced-usage/custom-rate-limits)
use a compact fixed-window contract for requests or cents and allow global,
user, or custom-property segmentation.

That interface is attractive for simple edge limits, but request-provided
headers and arbitrary properties are not an appropriate source of truth for
Chalk's admin-managed policies. Chalk also requires reservation and settlement
for token counts, which are unknown at admission time.

### 7.6 Infrastructure rate limiters

The [Envoy/Lyft rate-limit service](https://github.com/envoyproxy/ratelimit/blob/main/README.md)
is the closest prior art for the interface boundary. Callers submit a domain
and request descriptors over a stable gRPC API; the service matches
configuration and updates Redis-backed counters. It demonstrates that request
orchestration need not know the mutable-state implementation. Chalk differs by
compiling the matched policy before the backend call and by supporting
reservation settlement and concurrency leases, not only request counters.

[Kong's advanced rate-limiting plugin](https://developer.konghq.com/plugins/rate-limiting-advanced/)
likewise uses shared Redis state for consistent decisions across distributed
gateway instances. [Upstash's rate-limit library](https://upstash.com/docs/redis/sdks/ratelimit-ts/algorithms)
exposes several algorithms with explicit computational and burst tradeoffs.
Together these support keeping algorithm selection in a stable contract rather
than hard-coding one Postgres query path.

Redis Cluster still constrains the design: its
[official clustering documentation](https://redis.io/docs/latest/operate/oss_and_stack/management/scaling/)
requires every key touched by one Lua script to share a hash slot. That is why
this RFP names the environment-level hot-shard tradeoff instead of describing
Redis as an automatic scale-out change.

None of the reviewed LLM gateways or infrastructure services documents the
renewable concurrency-lease semantics Chalk needs for long-lived streams. That
part of the design is Chalk-specific and is justified by lifecycle correctness,
not competitive precedent.

## 8. Alternatives considered

### 8.1 Keep separate rate-limit and budget engines

Rejected. Both engines need atomic multi-policy acquisition, reservation,
settlement, backend selection, leases, error handling, metrics, and tests.
Duplicating that machinery increases semantic drift without preserving a useful
product distinction.

### 8.2 Replace both source tables with one quota table

Rejected. The existing source concepts remain separate even though
`rate_limits` gains the fields needed to become a complete definition. A
general user-facing quota DSL would also make a small enforcement change much
larger.

### 8.3 Aggregate `usage_events` during admission

Rejected. An analytics ledger is written after work completes and cannot
atomically reserve future usage across replicas. Querying it on every request
would also make admission cost grow with history.

### 8.4 Approximate every limit with a token bucket

Rejected. Token buckets are appropriate for continuously refilling rates, not
calendar ceilings or concurrency leases. One backend interface need not mean
one algorithm.

## 9. PR #173 implementation record

The following was the merge plan used for PR #173. It is retained as an
implementation record; subsequent migrations add named calendar periods and
unique source-policy identities rather than rewriting the already-applied quota
migration.

1. **Replace the schema.** Remove the PR's rate-specific state, reservation,
   lease, and stored-function schema. In one forward migration, change
   `budget_limits` to the composite primary key `(environment_id, id)`; add
   nullable `connection_id`, `models`, and `burst_value` to `rate_limits`;
   backfill exact legacy connection targets; normalize `token` scope to
   `api_key`; reject ambiguous legacy rows; and drop `target_type` and
   `target_value`. Add the checked `quota_policies` bridge and generic runtime
   tables. Do not edit the already-applied core migration.
2. **Separate policy stores from state mutation.** Refactor the current
   `RateLimitBackend` into a `RateLimitPolicyStore` that owns rate-limit CRUD
   and its bridge row. `QuotaPolicyResolver` compiles rate and
   budget definitions, performs selector matching and calendar-boundary
   calculation, then passes only matched actions to `QuotaBackend`.
3. **Replace budget admission rather than adding a second path.** Refactor
   `BudgetBackend` into a definition-oriented `BudgetPolicyStore`.
   `set_key_daily_budget`, `get_key_daily_budget`, and
   `get_key_daily_budgets` remain definition APIs and create/read the budget
   source plus bridge transactionally. Delete `BudgetBackend::check` and its
   `SUM(usage_events)` enforcement query; request admission uses only the budget
   arm of `QuotaBackend`.
4. **Implement common backends.** Retain the in-memory implementation and
   replace the Postgres rate-limit function with `acquire_many`, `finish`, and
   `renew` over compiled actions. Do not query source tables inside those
   operations.
5. **Wire both sources.** Acquire budgets and global rate limits together once
   at request admission. Acquire connection/model rate limits for every matching
   dispatch. Use explicit token-metered/request-only applicability and cover
   every upstream handler. Matching key/pool and broad/specific policies
   compose; none overrides another. Keep the rate-limit and budget admin labels
   separate.
6. **Consolidate configuration.** Add `ROUTER_QUOTA_BACKEND`; remove the old
   budget/rate-limit enforcement selectors and the unimplemented
   `chalk_service` and `clickhouse` quota stubs. Leave `ROUTER_USAGE_BACKEND`
   and its `clickhouse` ledger stub unchanged.
7. **Make unsupported definitions explicit.** Reject environment-wide policy
   scope, cost budgets, and rolling budgets. Remove or hide PR controls that
   imply support.
8. **Add bounded cleanup.** Run the Postgres reservation/lease janitor in
   bounded batches and test cold-policy cleanup independently of acquisition.
9. **Keep Redis as an interface target.** Reserve the config value and document
   key/atomicity requirements, but do not add a Redis dependency in this PR.
10. **Regenerate checked artifacts.** Run `just gen-proto` if protobuf changes
    remain, `just schema-write`, `just schema-check`, and the normal CI gate.

After PR #173, migrations `0019` and `0020` made usage budgets canonical per
owner and UTC period (`daily`, `weekly`, or `monthly`), with corresponding CRUD
and current-window state in the admin API. Migration `0021` canonicalized model
selectors, rejected duplicate rate-limit definitions, installed unique global
and connection-scoped identities, and removed the unused rate-limit name.
API-key and usage-pool views now manage their attached quota definitions.

At PR #173 merge time neither source had live enforcement state, so the
migration did not backfill counters or reservations. It backfilled bridge rows
for supported calendar token budgets and rate limits. A legacy rate limit had
to identify one existing connection and use supported scope, type, and value
semantics; the migration aborted rather than silently preserve an ambiguous
unenforced row.

## 10. Tests and observability

The shared backend conformance suite covers:

- the exact-one-source check, composite rate and budget foreign keys, cascades,
  and connection-delete restriction;
- database rejection of a budget bridge whose environment differs from the
  referenced budget;
- API-key and usage-pool matching, with no environment-scope path;
- global, connection-wide, and connection/model matching;
- cumulative key/pool and broad/specific policy behavior without implicit
  overrides;
- all-or-nothing acquisition across several policies;
- RPM refill and burst behavior;
- TPM reservation and actual-token refund;
- fixed calendar window reset, including timezone and anchor boundaries;
- concurrency acquisition, renewal, expiry, and release;
- reservation expiry and idempotent finish/renew behavior;
- bounded cold-policy janitor cleanup and concurrency-count repair;
- global request admission versus local dispatch admission through fallback
  and streaming;
- request-only operations omit token policies while retaining request and
  concurrency limits;
- provider-default image requests retain connection-wide rate limits and fail
  before dispatch when an applicable model-scoped limit requires an exact
  public model; model-less operations do not accidentally match model policies;
- embeddings and Responses compaction cannot bypass token budgets;
- cache hits consume request limits without consuming token budgets;
- fallback reservations use the largest viable route ceiling unless the client
  supplied a route-independent output cap, and any unbounded viable route makes
  that logical reservation explicitly unknown;
- unknown budgets and global/local TPM never compile zero-token actions or reach
  the quota backend;
- non-clean streams settle unknown usage before body drop when the terminal
  error is observed, while lease expiry covers abrupt disconnects;
- refund before dispatch and conservative retention after unknown usage;
- bounded, single-application settlement debt after under-reservation;
- explicit rejection of cost and rolling budget definitions;
- operation without reading or writing `usage_events`;
- backend error fail-open behavior distinct from HTTP 429 rejection; and
- failed non-atomic API-key provisioning revokes the credential first and
  reports every failed compensating cleanup, while PostgreSQL failures leave no
  partial key, source, or bridge rows.

Metrics use the source concept rather than backend table names:

- decisions by `source`, `algorithm`, `meter`, and outcome;
- acquisition and settlement latency by backend;
- active leases and expired-lease cleanup;
- backend errors and fail-open count; and
- conservative reservations retained because actual usage was unavailable.

Policy IDs, API-key hashes, and usage-pool IDs are logged only in the existing
approved redacted or hashed form. Raw API tokens are never quota labels.

## 11. Acceptance criteria

The merged implementation satisfies this RFP as follows:

1. Existing source IDs remain unchanged; `budget_limits` uses `PRIMARY KEY
   (environment_id, id)`, and `rate_limits` is the only rate-limit definition
   table with its optional selector and burst fields.
2. Every newly created source has one checked bridge row, and both bridge arms
   are protected by environment-qualified database foreign keys.
3. Memory and Postgres pass one backend conformance suite.
4. One request can atomically acquire all matching policies and a rejection
   consumes no capacity and creates no permit in any of them.
5. Calendar token budgets and RPM, TPM, and concurrency rate limits are enforced
   without querying `usage_events`.
6. The runtime backend receives compiled actions and does not load policy
   definitions itself.
7. Budgets and global rate limits are acquired together once per client request;
   connection/model rate limits are acquired per upstream dispatch. All
   matching key/pool and broad/specific policies must pass.
8. Cost, rolling budgets, and environment-wide policy scope fail validation
   explicitly.
9. Backend errors are observable and distinct from quota rejections.
10. `BudgetBackend::check` and its usage-ledger aggregation are removed, while
    budget definition reads and writes continue through `BudgetPolicyStore`.
11. `ROUTER_USAGE_BACKEND` remains independent and the unimplemented
    `chalk_service` quota option is removed.
12. Cold expired rows are reclaimed without waiting for another request on the
    same policy.
13. Every quota-covered endpoint uses the shared admission path; request-only
    operations cannot match token policies, and token-metered operations cannot
    bypass budgets.
14. Fallback admission reserves against every viable route, not only the
    primary route.
15. API-key provisioning never ignores revoke or definition-cleanup errors, and
    the normal PostgreSQL path remains atomic.
16. The Redis implementation can be added behind `QuotaBackend` without a
    source-table migration or a change to request orchestration.
17. Provider-default image requests cannot bypass model-scoped rate limits:
    applicable model-scoped definitions require an explicit model, while
    connection-wide limits continue to work without one.
