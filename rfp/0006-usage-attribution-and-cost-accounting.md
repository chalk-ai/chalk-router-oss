# RFP-0006: Usage Attribution and Token Accounting

- **Status:** Historical — durable accounting implemented; agent dimension superseded
- **Created:** 2026-08-28
- **Revised:** 2026-09-03 — clarified which decisions remain live after removal
  of agent attribution
- **Superseded in part by:** [PR #169](https://github.com/chalk-ai/chalk-router/pull/169)
  removed the `agent_id` usage dimension; [RFP-0007](0007-backend-neutral-quota-enforcement.md)
  defines current admission-time token budgets and rate limits
- **Depends on:** [RFP-0001](0001-router-motivation-and-architecture.md) —
  API keys, connections, routing, usage accounting, and budgets
- **Scope:** Give Chalk's token-usage ledger stable attribution, explicit
  provenance, bounded aggregate queries, and retry-safe per-call persistence.
  Promote create-only API-key `labels`, reject deprecated `cost_tags`, add one
  `agent_id` dimension, and preserve flat token pools.
- **Non-goals:** Invoice generation, customer billing, monetary accounting,
  nested workspace/organization policy, prompt/response retention, mutable
  request metadata, or post-completion attribution edits.

> The router no longer persists or queries an `agent_id` usage dimension.
> Federated usage is identified by its synthetic `federated:{sub}` API-key
> hash; the design below is retained as historical context.

The durable per-call ledger, immutable API-key labels, deprecated `cost_tags`,
token normalization/provenance, bounded aggregate query, and PostgreSQL source
of truth described below are implemented and remain current. Sections and
acceptance criteria concerning `agent_id` are superseded and must not be used as
the current contract. RFP-0001 §9 is the concise living architecture summary.

---

## 0. Summary

Chalk already records token usage by API key, optional usage pool, connection,
and model. The current implementation is useful but is not yet a reliable
accounting substrate:

- API keys expose both create-only `labels` and mutable `cost_tags`, even though
  `cost_tags` do not reach the usage ledger and should not imply a second
  metadata contract.
- A federated JWT subject is encoded into a synthetic API-key hash instead of
  being represented as an independent agent identity.
- Usage rows use flush time rather than request completion time.
- An ambiguous database failure can lose a drained aggregate or duplicate a
  retry.
- A response that omits part of its usage can silently become zero.
- Query behavior is split between API-key snapshots and a legacy histogram.

This RFP makes six decisions:

1. **Keep immutable API-key `labels`.** Labels remain the only arbitrary
   key/value metadata map. They are configured only when a key is created.
   There is no canonical `tags` field and no request-tag contract.
2. **Reject deprecated `cost_tags`.** Keep its protobuf field numbers for wire
   compatibility, mark the fields deprecated, reject non-empty create/update
   input, return it empty, and remove it from the UI.
3. **Use one identity, `agent_id`.** Human users, services, SDK agents, and
   automated workloads share one opaque attribution dimension. HTTP
   `User-Agent` is not an identity.
4. **Normalize or estimate token usage explicitly.** Valid response usage is
   authoritative. A conservative estimator fills supported text-only gaps.
   Every event records `reported`, `estimated`, or `legacy_unknown` provenance.
5. **Persist one idempotent event per completed call.** A stable `event_id`
   survives retries. Stored `request_count` and `total_tokens` have identical
   aggregation semantics for historical and new rows.
6. **Attribute events to immutable connections, not adapter families.** New
   rows store `connection_id` and model, but do not emit the deprecated
   `provider` column. Provider is not a query dimension or filter. The legacy
   histogram provider field remains on the wire as deprecated and empty.

Labels are not copied into the ledger. A label breakdown is computed by
selecting immutable labeled API keys, aggregating their events by key, and
folding those aggregates by label value.

## 1. Normative decisions

### 1.1 Credential metadata

- `description` remains mutable human-readable administration metadata.
- `labels` is the only arbitrary key/value map in the API-key contract.
- Labels are supplied on create and are immutable for the key's lifetime.
- Protobuf `cost_tags` remains deprecated. Non-empty inputs are rejected,
  responses leave it empty, and the UI does not expose it.
- Labels continue to use the existing database `labels` column.
- There is no canonical `tags` field, request tags, or usage-event tags.
- The legacy `cost_tags` database column is never read by normal service paths
  or joined into usage queries.

### 1.2 Label-based usage views

- Events are attributed to `api_key_hash`, not copied API-key labels.
- Filtering by labels first resolves matching keys from control-plane metadata,
  then constrains the ledger query to those hashes.
- Grouping by one label key queries the ledger by API key and folds aggregates
  by immutable label value in the service layer.
- Missing labels are omitted from that breakdown.
- Grouping never mutates or backfills historical events.

### 1.3 Agent attribution

- The only identity dimension added by this RFP is `agent_id`.
- There is no separate `user_id`, `customer_id`, `app_id`, or `principal_id`
  usage column.
- Verified authentication identity outranks request-supplied identity.
- HTTP `User-Agent` remains diagnostic client-software metadata and must never
  populate `agent_id`.
- Attribution is resolved once before dispatch and snapshotted on the event.

### 1.4 Connection attribution

- `connection_id` is the durable upstream-routing dimension.
- The adapter-family/provider kind is mutable connection metadata and is not
  copied into new events.
- The existing `usage_events.provider` column becomes nullable, receives a
  database deprecation comment, and is omitted by new writers.
- Existing provider values remain untouched for rollback compatibility.
- No usage query, grouping, budget read, or index depends on `provider`.
- Provider-kind reporting, if later required, must join immutable connection
  identity to current connection metadata and clearly describe those semantics.
- The legacy histogram `provider` field stays at field number 1, is marked
  deprecated, and is returned as an empty string.

### 1.5 Usage and estimation

- Every successful generation request participating in accounting produces a
  finalized token total or fails its accounting boundary visibly.
- Valid response usage is authoritative and replaces provisional estimates.
- Estimated token counts are explicitly persisted as `estimated`.
- Missing cache or reasoning detail is zero, not fabricated.
- Cache read/write values are subsets of prompt tokens.
- Reasoning tokens are a subset of completion tokens.
- Built-in adapters have fixture-covered extraction.
- Arbitrary compatible connections must report valid usage or opt into a
  supported estimator before claiming complete accounting.
- The first implementation does not add a tokenizer dependency.

### 1.6 Persistence and arithmetic

- `usage_events` becomes an append-only per-call ledger for new writes.
- The existing `time` column records completion time for new events.
- There is no second completion timestamp and no `row_kind` column.
- Each call receives a stable UUID `event_id` at request acceptance.
- Retry uses the same event ID and conflicts on `(environment_id, event_id)`.
- Every new call stores `request_count = 1`.
- Every new call stores `total_tokens = prompt_tokens + completion_tokens`.
- Every reader sums stored `request_count` and `total_tokens` directly.
- Historical aggregates keep their stored counters and get
  `usage_source = 3` (`USAGE_SOURCE_LEGACY_UNKNOWN`).
- Calls may be inserted in batches, but may not be combined before persistence.

## 2. API-key labels and deprecated `cost_tags`

### 2.1 Protobuf contract

Keep established label fields and deprecate the old second map without reusing
its field numbers:

```proto
message ApiKey {
  map<string, string> labels = 5;
  map<string, string> cost_tags = 8 [deprecated = true];
}

message CreateKeyRequest {
  map<string, string> labels = 3;
  map<string, string> cost_tags = 5 [deprecated = true];
}

message UpdateKeyOperation {
  map<string, string> cost_tags = 4 [deprecated = true];
}
```

There is no replacement `tags` field.

### 2.2 Lifecycle and validation

Labels are accepted only during create and are never update-mask fields. Use
bounded validation:

- at most 32 entries;
- UTF-8 key length 1–64 bytes;
- UTF-8 value length 0–256 bytes;
- total encoded key/value bytes at most 4 KiB;
- reject reserved `chalk.` key prefixes; and
- reject control characters.

Any non-empty deprecated map on create, or any update attempt addressing that
field, returns `INVALID_ARGUMENT`. Reads and lists return it empty. Existing
physical column contents are ignored rather than mapped into labels.

### 2.3 Label selection

Label filters use ordinary JSONB equality predicates. GIN is not required and
must not be introduced. Candidate selection is capped at 1,000 API keys per
query. Label grouping then performs one ledger query grouped by key followed by
checked service-layer folding.

## 3. Agent identity resolution

Resolve at most one opaque agent ID with this precedence:

1. verified federated JWT or client-credential subject;
2. a validated Chalk attribution header;
3. a supported protocol-native user field; or
4. absent.

Reject conflicting simultaneously supplied values. Consume Chalk-only headers
before upstream dispatch. Validation permits 1–256 UTF-8 bytes and rejects
control characters. The raw authorization token, API-key secret, and unverified
JWT claims never become identity.

## 4. Token normalization

### 4.1 Canonical shape

Normalize each completed call to:

```text
prompt_tokens
completion_tokens
cached_tokens
cache_write_tokens
reasoning_tokens
total_tokens
usage_source
```

`total_tokens` is always the checked sum of prompt and completion. Reported
totals are consistency signals, not an independent additive category.

### 4.2 Provenance codes

Define the canonical values in protobuf and persist those numeric values in a
PostgreSQL `SMALLINT`:

```proto
enum UsageSource {
  USAGE_SOURCE_UNSPECIFIED = 0;
  USAGE_SOURCE_REPORTED = 1;
  USAGE_SOURCE_ESTIMATED = 2;
  USAGE_SOURCE_LEGACY_UNKNOWN = 3;
}
```

```sql
usage_source SMALLINT NOT NULL DEFAULT 3
    CHECK (usage_source IN (1, 2, 3))
```

Zero is not persisted. The database codes deliberately match the protobuf enum
numbers so storage, generated clients, and application constants share one
stable representation without a PostgreSQL enum type.

- `reported`: derived from a valid response or terminal stream event.
- `estimated`: derived by a supported conservative estimator.
- `legacy_unknown`: historical data whose provenance cannot be reconstructed.

Token provenance is not exposed as a monetary or billing claim.

### 4.3 Estimation

Use exact response usage whenever available. For supported text-only calls
without valid usage, a conservative byte-based estimator may emit bounded
prompt and completion totals. It records no invented cache/reasoning detail.

Unsupported modalities or arbitrary compatible connections without declared
semantics fail the accounting boundary instead of recording silent zeroes.
Estimator quality is tested against response-reported fixtures, but no
tokenizer package is added in this implementation.

## 5. Durable event ledger

### 5.1 Logical event

One event contains:

```text
event_id
time
environment_id
api_key_hash
usage_pool_id
agent_id
connection_id
model
prompt_tokens
completion_tokens
cached_tokens
cache_write_tokens
reasoning_tokens
total_tokens
request_count
usage_source
```

The event contains neither prompts/responses nor arbitrary maps. The deprecated
physical provider column is not part of the logical event.

### 5.2 Event identity

Allocate a random UUID when the request is accepted. Retain it through routing,
streaming, finalization, and database retries. Insert with:

```sql
ON CONFLICT (environment_id, event_id) DO NOTHING
```

The upstream request ID may be stored separately for diagnostics later, but is
not suitable as the ledger idempotency key.

### 5.3 Safe migration sequence

Large ledgers require separately committed schema, backfill, index, and
validation work:

1. add nullable `event_id`, add the `usage_source` smallint with metadata-only
   `USAGE_SOURCE_LEGACY_UNKNOWN` (`3`) default and a range check, make
   `provider` nullable, and comment it deprecated;
2. backfill only rows whose event ID is null;
3. build the unique `(environment_id, event_id)` index concurrently;
4. build the partial connection/time index concurrently; and
5. validate event/token invariants and finalize `event_id NOT NULL`.

Concurrent-index migrations are non-transactional and use the router's exact
index-definition recovery metadata. No GIN or extension-owned index is added.

### 5.4 Writer behavior

The writer inserts one event with `connection_id`, model, stored counters, and
provenance. It omits `provider`. A successful terminal response waits for its
usage write; disconnect-triggered finalization is drained on shutdown. Retries
are bounded and retain the event ID.

## 6. Query API

### 6.1 Explicit grouping

Provider is not an active dimension. Because this API is introduced by this
unmerged stack, its enum and message fields use a clean numbering scheme with
no compatibility reservations:

```proto
enum UsageDimension {
  USAGE_DIMENSION_UNSPECIFIED = 0;
  USAGE_DIMENSION_CONNECTION = 1;
  USAGE_DIMENSION_MODEL = 2;
  USAGE_DIMENSION_API_KEY = 3;
  USAGE_DIMENSION_USAGE_POOL = 4;
  USAGE_DIMENSION_AGENT = 5;
}

message QueryUsageRequest {
  google.protobuf.Timestamp start = 1;
  google.protobuf.Timestamp end = 2;
  string bucket_size = 3;

  oneof grouping {
    bool global = 4;
    UsageDimension dimension = 5;
    string api_key_label = 6;
  }

  UsageFilters filters = 7;
}
```

`global = true` aggregates across all dimensions within each time bucket.
`dimension` selects one built-in dimension. `api_key_label` is the immutable
API-key label key used for post-aggregation. Missing grouping, an unspecified
dimension, `global = false`, and an empty or invalid label key are rejected.

### 6.2 Filters

```proto
message UsageFilters {
  repeated string api_key_hashes = 1;
  repeated string usage_pool_ids = 2;
  repeated string agent_ids = 3;
  repeated string connection_ids = 4;
  repeated string models = 5;
  map<string, string> api_key_labels = 6;
}
```

### 6.3 Aggregate response

Keep only token and request counters. Because no released API used the
abandoned cost fields, number the message contiguously without reservations:

```proto
message UsageAggregateRow {
  google.protobuf.Timestamp bucket = 1;
  string dimension_value = 2;
  int64 prompt_tokens = 3;
  int64 completion_tokens = 4;
  int64 cached_tokens = 5;
  int64 cache_write_tokens = 6;
  int64 reasoning_tokens = 7;
  int64 total_tokens = 8;
  int64 request_count = 9;
}
```

All aggregate arithmetic is direct `SUM` over stored counters. Token provenance
remains on the stored usage event and is not exposed by the aggregate query API.

### 6.4 Bounds

- start is inclusive and end is exclusive;
- time range is at most 366 days, supporting rolling years and leap calendar years;
- minute buckets are restricted to short ranges;
- intermediate and final results are capped at 10,000 rows;
- label selection is capped at 1,000 keys; and
- every caller value is bound, never interpolated.

### 6.5 Legacy histogram

The existing histogram remains for compatibility but groups by connection and
time only. Its provider field is retained and deprecated:

```proto
message UsageHistogramRow {
  string provider = 1 [deprecated = true];
  google.protobuf.Timestamp bucket = 2;
  int64 total_tokens = 3;
  string connection_id = 4;
  // remaining token counters unchanged
}
```

The server emits an empty provider string. Clients should use `connection_id`.

## 7. Budgets

Budget enforcement remains token-only. Existing daily API-key budgets and flat
usage-pool token budgets read direct sums of stored `total_tokens`. There is no
workspace/organization hierarchy and no label-, agent-, model-, or
connection-specific budget inheritance in this RFP.

## 8. UI

The API-key UI:

- accepts labels only at key creation;
- displays labels on the one-time issuance result;
- never offers label editing; and
- never renders deprecated `cost_tags`.

The usage UI supports breakdown by connection, model, API key, usage pool,
agent, or one API-key label. It does not offer provider, monetary, or token
provenance columns.

## 9. Observability

Useful counters include:

```text
router_usage_records_total{protocol,usage_source}
router_usage_estimation_total{protocol,method}
router_usage_normalization_error_total{protocol,reason}
router_usage_write_retry_total{reason}
router_usage_write_failure_total{reason}
```

Do not put API-key hashes, agent IDs, connection IDs, model IDs, or label values
in metric labels. Structured logs may contain connection ID, model, event ID,
and bounded failure context, but never raw credentials or prompts.

## 10. Rollout plan

### Phase 1: Contract and identity

- Promote immutable create-only labels.
- Deprecate and reject `cost_tags` without adding replacement tags.
- Add conflict-checked `agent_id` resolution and snapshotting.

### Phase 2: Durable token accounting

- Add event identity and the protobuf-numbered usage-source smallint.
- Deprecate the physical provider column and stop new writes to it.
- Backfill IDs separately and build indexes concurrently.
- Replace process-local 60-second aggregation with idempotent per-call writes.
- Normalize response usage and estimate supported text gaps.

### Phase 3: Bounded reporting

- Add the bounded query RPC.
- Require an explicit global, built-in-dimension, or API-key-label grouping.
- Add connection/model/key/pool/agent/label dimensions and filters without a
  provider field.
- Implement label post-aggregation and the token-only UI.

### Follow-ups

- Remove deprecated protobuf fields and physical columns after compatibility
  telemetry and a published removal window.
- Add a scale-oriented analytical replica only after PostgreSQL query evidence
  requires it; PostgreSQL remains the durable source of truth.
- Add tokenizer-specific estimators only when fixture evidence demonstrates
  that the conservative fallback is inadequate.

## 11. Test plan

### 11.1 Labels

- Create accepts valid labels and rejects malformed or oversized maps.
- Labels cannot be changed through update masks or operation fields.
- Non-empty deprecated map input is rejected.
- Reads return deprecated maps empty and the UI omits them.
- Label grouping selects bounded matching keys and performs no N+1 queries.

### 11.2 Agent attribution

- Verified identity outranks request identity.
- Conflicting identities fail before dispatch.
- HTTP `User-Agent` never becomes `agent_id`.
- Chalk-only headers are not forwarded upstream.

### 11.3 Token normalization

- Chat, Responses, Anthropic, Bedrock, and compatible response fixtures cover
  their valid usage shapes.
- Cache and reasoning subsets are never added twice.
- Reported totals replace provisional estimates.
- Supported missing text usage is marked `estimated`.
- Unsupported missing usage fails visibly.

### 11.4 Ledger and migrations

- Every new call stores a unique event ID and `request_count = 1`.
- Retrying the same event ID cannot double count.
- Historical rows receive unique IDs and `legacy_unknown` provenance.
- Stored usage-source codes match the protobuf enum numbers and reject zero or
  unknown values.
- New writers omit provider while existing provider values survive migration.
- The provider column is nullable and carries its deprecation comment.
- Concurrent indexes recover only from exact valid definitions.
- All readers sum stored `total_tokens` and `request_count` uniformly.

### 11.5 Query API

- Provider dimension and filter fields are absent from the new query API.
- Missing grouping, an unspecified dimension, false global grouping, and an
  invalid label key are rejected.
- Global grouping returns one aggregate per time bucket.
- Connection, model, key, pool, agent, and label dimensions work.
- The new query messages contain no abandoned cost fields or reservations.
- Legacy histogram returns provider empty and connection populated.
- Bounds and row caps reject unsafe queries.

## 12. Acceptance criteria

1. `labels` is the only active arbitrary API-key key/value metadata map and is
   immutable after creation.
2. Deprecated `cost_tags` input is rejected and never shown in the UI.
3. Usage contains one conflict-checked `agent_id` and no user/request tag maps.
4. Every new completed call has a stable event ID and durable idempotent row.
5. Token source is `reported`, `estimated`, or `legacy_unknown`, stored as the
   matching protobuf enum number in a checked `SMALLINT`.
6. `time` is the only event timestamp and no `row_kind` exists.
7. Stored `request_count` and `total_tokens` have uniform query semantics.
8. New events contain connection/model attribution but omit provider.
9. The query API has no provider grouping or provider filter.
10. Provider indexes do not exist and the legacy histogram field is deprecated
    and empty.
11. Query grouping is an explicit oneof and aggregate fields are contiguous.
12. Token budgets remain flat and token-denominated.
13. No tokenizer dependency, GIN index, prompt retention, or hierarchy is added.

## 13. Stacked implementation plan

1. **PR 138 — RFP:** define immutable labels, agent attribution, provider-free
   token events, provenance, split migration rollout, and bounded queries.
2. **PR 139 — labels:** keep create-only labels, reject deprecated `cost_tags`,
   remove that UI input, and add no canonical tags.
3. **PR 140 — agent attribution:** resolve and snapshot only `agent_id`.
4. **PR 141 — durable ledger:** add event IDs and the protobuf-numbered
   usage-source smallint, split the large-table rollout, stop provider emission,
   and use uniform counters.
5. **PR 142 — normalization:** harden valid response usage and conservatively
   estimate supported missing text usage without adding a tokenizer.
6. **PR 143 — query:** add explicit bounded global/connection/model/key/pool/
   agent/label reporting and keep the new query protobuf free of abandoned
   provider and cost fields.

The stack ends at PR 143.
