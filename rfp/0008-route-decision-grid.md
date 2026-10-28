# Route-owned judging and fallback

## Behavior

Each model route owns its judge and fallback configuration. A simple route uses
its top-level provider connection and upstream model. Custom routing requires its
own `RouteGrid.default_target`, containing a connection ID and upstream model ID;
it never inherits the simple-route fields. Internally, `ModelRoute.routing` is
`Option<RouteGrid>`: `None` means a simple route, and every `RouteGrid` contains
a concrete `RouteTarget`. The storage boundary maps existing empty JSON objects
to `None` and writes clears as `{}`, preserving the database schema. Partial
updates distinguish leaving routing unchanged, clearing it, and replacing it.
That destination is chosen when
judging is off, in shadow mode, or unavailable; it is not a capability ceiling.
Judging applies to Chat Completions, Anthropic Messages, and self-contained OpenAI
Responses create requests, for both streaming and non-streaming delivery. Route
fallbacks also apply to Decisions/System One. Other endpoints retain their
single-target behavior. Request-specific fallback overrides remain a Chat
Completions feature.

A live judge classifies the request by task or difficulty. The configured outcome
selects a concrete model. The ordered attempt list consists of that model followed
by its applicable alternatives. Fallback may move to any explicitly configured
model; the previous cheaper-only ladder substitution is removed.

The backend supplies starter criteria through `GetRouteJudgeDefaults`: difficulty
labels `0` and `1`, and task labels `coding`, `summarization`, `extraction`, and
`other`. The editor starts new categories at the route's concrete default target
with inherited fallback behavior. Existing descriptions and mappings are preserved.
For remote judges, blank descriptions use the backend criteria for standard task
labels or the two-level difficulty scale; custom labels and larger difficulty
scales require their own criteria. Local heuristics ignore descriptions and still
use their configured keyword, code-block, and length signals.

Chat request fallback configuration is a map from an initially selected concrete
model to its alternatives. A missing key uses route configuration; a present key
with an empty array disables alternatives for that selection. The lookup happens
once. Reaching another model never consults that model's map entry, invokes a
second judge, or resumes a different chain after exhaustion. Request overrides do
not modify stored route configuration or reach upstream providers.

Request fallback keys and alternatives accept `connection_id/upstream_model_id`
or `connection_name/upstream_model_id`. Connection names are exact and
case-sensitive, and are resolved to IDs together from one connection snapshot
before selection. IDs take precedence over names; if both forms key the same
selected model, the ID-keyed entry wins (including an empty list). Unknown keys
remain unmatched. Duplicate destinations and self-loops are rejected after name
resolution too. Stored route policies continue to use IDs, so renaming a
connection only requires updating requests that refer to its old name.

Route configuration has a default model-to-alternatives map. Each classification
outcome can optionally supply its own alternatives. Selection order is:

1. The request's entry for the selected model, when present.
2. The selected classification outcome's alternatives, when explicitly present.
3. The route's default entry for that model.
4. No alternatives.

Omitted outcome alternatives inherit; an explicit empty array stops. Lists are
never merged. Two outcomes selecting the same model can recover differently.

For example, a route defaults to `openai-connection/astra`. Its easy outcome
selects `openai-connection/gpt-4o`, whose alternatives are
`[openai-connection/gpt-5.5, anthropic-connection/opus]`. If gpt-4o returns 503,
gpt-5.5 is attempted. Its own map entry is never expanded. A request with
`{"fallbacks":{"openai-connection/gpt-4o":["openai-connection/astra"]}}`
replaces only that request's easy-selection alternatives. An override keyed only
by gpt-5.6 does not affect the easy-selection chain.

## Destinations and authorization

Policies refer to concrete provider connections and upstream models, not other
route aliases. Default targets and remote classifier targets use `RouteTarget`,
with separate `connection_id` and `upstream_model_id` fields. Outcome models and
fallback references use `connection_id/upstream_model_id`; the first slash
separates the stored connection ID, and the remaining text is the upstream model
ID (which may contain slashes). IDs are case-sensitive. Connection IDs are
stable store IDs, not display names or routing prefixes. The example IDs above
are placeholders. Connections with explicit exposure are supported.

A published alias and its default connection must resolve before entering the
grid. The original request permission gate remains terminal. Every selected or
fallback destination is independently resolved, checked for model permission,
egress and capabilities, and dispatched using that exact resolved backend.
Alternative aliases are rejected. Missing, disabled, denied, duplicate, or
incompatible candidates are skipped. A missing judge-selected target can still
use its configured alternatives. Deleting another route cannot invalidate a
policy; deleting a connection warns about concrete policy references.

The route's default and grid are captured together from a routing snapshot.
Updating a policy does not alter an in-flight request's captured grid. Candidate
connections are resolved before admission and retained through dispatch; they
are not looked up again between authorization and use. Existing snapshot refresh
behavior controls when a new request sees a route edit.

## Failures, limits, and accounting

Fallback triggers are connection failures/timeouts, upstream HTTP 429/5xx, and
candidate-specific rate-limit exhaustion. Other upstream errors, unreadable
provider responses, refusals, and answer-quality failures are terminal. Shared
key/pool quota denial stops the request. Once a streaming response is committed,
subsequent failures terminate that stream and never invoke another target.

Validation limits each list to 16 alternatives, each route/request map to 128
entries, and each judge to 32 outcomes. A list cannot repeat its starting model
or an alternative. Runtime also deduplicates resolved destinations. The entire
attempt phase has a 300-second deadline, in addition to existing stream-open
and compatibility deadlines. Remote judge classification has its own 1–30,000 ms
bound, including resolution and quota admission. Completion and Decisions backend
configs each own `timeout_ms`; omission or zero uses the default of 1,000 ms,
and zero does not disable the deadline. The heuristic has no configurable timeout.
On timeout, selection uses the route's default target and its applicable fallbacks.
The shared runtime admits at most 32 simultaneous classifications without waiting.

One logical request quota reservation covers the generation plan, using the
most conservative eligible candidate estimate. Unknown estimates stay unknown.
Each dispatched attempt also observes its own connection/model rate limits.
Failed dispatches and partial streams retain the existing conservative settlement
and cancellation cleanup behavior.

Remote judging consumes auxiliary token budgets and applicable connection/model
rate limits, but does not count as another logical user request. Known judge
usage settles through the existing usage pipeline even if its classification is
malformed. Failed or cancelled judging retains conservative quota accounting.
Shared token-budget rejection is terminal; unavailable classification, unknown
judge reservations, or judge connection admission failure select the default.

## Conversation endpoint integration

The shared judge input contains the endpoint identity and its original request.
Each endpoint supplies a bounded classification view of its native conversation,
instructions, tools, and output requirements. Provider dispatch retains the
original request: enabling a judge does not force native Messages or Responses
through the stricter Chat compatibility codec. Candidate eligibility is still
checked against the original endpoint and request, including native-only features.

The local heuristic reads conversation text; remote judges receive the bounded
native shapes. The view keeps a leading Chat instruction and the last eight
conversation items, clips strings, and bounds collections, nesting, and total
content. Embedded binary data and encrypted reasoning are omitted. This is a
text-classification view, not a multimodal understanding guarantee.

Responses that reference `previous_response_id`, `conversation`, or a stored
`prompt` skip both live and shadow judging because the router does not have the
referenced context. They choose the route default and retain its fallback list,
subject to existing candidate eligibility and response-locator connection-affinity
checks. No stored history is fetched and no session affinity is added. Compaction,
retrieval, deletion, and Messages token counting do not invoke a judge.

A future conversation endpoint can provide its own classification view and
external-context guard while reusing selection, fallback, and quota handling.
Its transport, capabilities, and continuation semantics still need endpoint-specific
integration; Gemini Interactions itself is not implemented in this increment.

## Judging and caching

The backend configuration owns its classifier target: `HeuristicJudgeBackend`
has no model, while `CompletionJudgeBackend` and `DecisionsJudgeBackend` require
a concrete `RouteTarget` and own their timeout setting. `RouteJudge` owns mode,
classification, and outcomes. The admin protocol represents backend alternatives
with a `oneof`; backend edits replace the transport and its settings together.
An unspecified mode defaults to Off; an unspecified classification is rejected
whenever a judge is configured.

Heuristic judging supports binary difficulty labels `0` and `1`. Its default
keyword set can be disabled, extended, or reduced. Keywords match user-message
text as case-insensitive substrings; a keyword cannot be both added and removed.
Code fences in any message role count as hard by default and can be disabled.
The length signal measures total UTF-8 text bytes in the bounded classification
view across message roles: exceeding 6,000 bytes is hard by default, and a zero
cutoff disables this signal. Any enabled hard signal selects `1`; otherwise `0`.
Keyword overrides are limited to 256 entries combined, each nonblank and at most
256 bytes. These settings are part of judge cache identity.

Remote
Completion and Decisions transports support named task labels or consecutive
difficulty labels `0..N-1`. Model capability/cost ordering is not inferred.
Unknown labels, malformed/ambiguous verdicts, errors, and deadlines choose the
default. Off never classifies. Shadow starts classification after successful
serving admission, records the proposed model and applicable fallback list,
and sends no generation requests to proposed targets. Remote shadow work still
consumes its own auxiliary token budget.

Successful verdicts are cached for 60 seconds, with at most 1024 entries. Identity
includes the endpoint identity, authenticated key, complete request (before
classification clipping), complete judge configuration,
concrete judge target, and resolved backend identity. Changed backend credentials
or configuration cannot reuse the prior backend's verdict. Failed classifications
are not cached. Egress is checked before cache lookup and candidate permissions
are checked after selection. No session-affinity behavior is introduced.

Response caching is bypassed on routes with a routing policy and on requests
with fallback overrides, so a cached served response cannot bypass a new plan.
Simple routes retain their existing response caching.

## Backend increment and deferred work

The backend exposes typed route configuration through ModelRouteService; updates
replace the whole routing field atomically. Selecting an absent routing field in
an update mask clears it. Unselected fields are preserved. Global judge/fallback
RPCs return FAILED_PRECONDITION pointing to route configuration. Global judge
startup/probe work and request-time global policy reads are retired. Archived
policy stores and schema remain for later cleanup; no old policy is silently
converted. Legacy per-key judge opt-in/floor values and global judge environment
settings do not participate in this route-owned execution path.

Both the in-memory and PostgreSQL stores support these policies. PostgreSQL stores
the complete grid in the model route's `routing` JSONB column, added by migration
0024 in the separate proto/schema PR. Apply that migration before running this
backend against a database. Existing rows with `{}` decode as `routing: None`;
unrelated updates preserve routing, while an explicit clear removes the grid
and writes `{}`. An explicitly supplied grid must contain a default target.
Malformed stored configuration returns a persistence error rather than silently
selecting a different policy. Shared conformance tests cover both stores, and
PostgreSQL tests also verify reloads through a fresh connection pool.

The UI route editor remains deferred; configure grids through ModelRouteService
in the meantime. Generated admin protocol bindings are updated with the backend
API. The old experiments remain preserved in their stash.
