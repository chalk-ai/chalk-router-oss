# RFP-0002: Streaming Fallback (retry-before-first-byte)

> **Reading note (2026-10-26):** The motivation and broad error-retry rules below
> describe the proposal at its revision dates. Streaming fallback is implemented;
> current eligibility is narrower than “every post-dispatch failure.” See
> [model routing](../docs/model-routing.md) for client errors, stream commitment,
> route/request precedence, and background-submission uncertainty. The original
> design rationale is retained below.

- **Status:** Accepted / Implemented
- **Created:** 2026-07-23
- **Revised:** 2026-08-27 — §5.3 now matches cross-model fallback semantics used
  by OpenRouter and LiteLLM: every post-dispatch upstream failure advances an
  explicit chain; router-owned pre-dispatch verdicts remain terminal or skipped.
- **Revised:** 2026-09-03 — relabeled the point-in-time implementation baseline
  and refreshed the final code location for shared fallback orchestration
- **Depends on:** [RFP-0001](0001-router-motivation-and-architecture.md) — §Routing, §Fallback policy, §API-key permissions
- **Scope:** Extend the model fallback policy to **streaming** chat completions, up
  to the point the HTTP 200 is committed. Two supporting changes are in scope
  because streaming fallback is not useful without them: a **per-attempt
  stream-open deadline** (§5.4) and a shared boundary between upstream dispatch
  failures, which may advance an explicit cross-model chain, and router-owned
  pre-dispatch verdicts, which do not (§5.3).
- **Non-goal:** Mid-stream failover.

---

## 1. Summary

Today the fallback policy only rescues **non-streaming** chat. Because most chat
traffic is streaming, a `chalk/*` route's fallback list is effectively inert for
the majority of real requests. This RFP extends fallback to the streaming path
by retrying alternates in the window *before* the router commits its `200 OK` —
a clean, client-invisible retry — while leaving mid-stream failover out of scope.

## 2. Motivation

- Fallback is a headline routing feature (RFP-0001 §Fallback policy) but silently
  does nothing for streaming, which is where the traffic is. That's a surprising,
  undocumented gap: `chalk/*` aliases route stably for streaming but never fail over.
- The fix reuses machinery that already exists and is tested for non-streaming:
  strict alternate resolution and per-attempt permission filtering. The new work
  is threading it through the stream-open path, re-stamping attribution from the
  winner, and bounding each attempt in time.

## 3. Pre-implementation baseline (historical)

This section records the code shape before RFP-0002 was implemented. Its line
numbers are point-in-time review references, not current source navigation; the
current shared orchestration lives in `src/handlers/routing.rs` and is called by
both streaming and non-streaming chat paths.

Fallback lives only in `try_with_fallbacks` (`src/handlers/chat.rs:120`), invoked
from `handle_non_streaming`. The **streaming** path, `handle_streaming`
(`chat.rs:373`), does:

1. Resolve the route **once** up front (`state.backend.resolve(&original_model)`)
   — for attribution and the permission gate; an unresolvable model fails here as
   a real non-2xx.
2. Stamp the span from that resolution *immediately* (`chat.rs:411`), before the
   gate and before dispatch, so a denied or errored stream span still carries
   `llm.system` / `llm.model_name` / connection identity.
3. Permission gate (RFP-0001 §API-key permissions): a denied primary is a terminal `403`.
4. **Model-as-judge** (`crate::judge::route`, `chat.rs:454`): an opted-in key's
   request may be downgraded to a cheaper ladder rung, which replaces the route
   and sets `x-chalk-routed-model`. The returned `Downgrade.cheaper_fallbacks`
   is **discarded** here — the code comment at `chat.rs:450-452` says so
   explicitly, deferring to this RFP.
5. `create_chat_completion_stream(req)`:
   - **`Ok(stream)`** → build a `200 text/event-stream` response and pipe the bytes.
   - **`Err(e)`** → `backend_error_response(&e)` — a real non-2xx; **no 200 has been committed yet**.

The key fact that makes this safe: `create_chat_completion_stream` opens the
upstream request and **checks status before returning**. There are exactly three
implementations in the tree, and all three satisfy this:

| impl | behavior |
|---|---|
| `OpenAICompatibleBackend` (`src/backend/openai_compatible.rs:207-215`) | non-2xx → `Err(ApiError{status,headers})` before `bytes_stream()` |
| `AnthropicBackend` (`src/backend/anthropic.rs:681-688`) | same |
| `RouterBackend` (`src/backend/router.rs:811`) | pure delegation to a leaf |

Every other provider — fireworks, groq, together, vllm, xai, grok, qwen, mistral,
cohere, openrouter, bedrock — is a thin `OpenAICompatibleBackend::for_provider(...)`
constructor, so it *is* the OpenAI adapter. The default trait implementation
(`src/backend/provider.rs:205`) buffers a non-streaming call and synthesizes SSE;
it is currently unused, and is also `Err`-before-bytes by construction. So the
property holds tree-wide today.

That gives a well-defined **retry window**: between "we decided to stream" and
"we returned `Ok(stream)` and committed 200." A failure inside that window is
indistinguishable, to the client, from a non-streaming failure — so we can fall
back transparently. After 200 is committed we are past the point of no return.

One property the original code did **not** have, and which streaming fallback
needs (§5.4):

- **No timeout of any kind.** `src/main.rs:214` is a bare `reqwest::Client::new()`
  — no `timeout`, no `connect_timeout`. A stalling upstream hangs until the OS
  gives up on the socket.

## 4. Goals / non-goals

**Goals**
- Apply the fallback list to streaming chat: if the primary's stream fails to
  *open*, try each alternate before committing a 200.
- Reuse existing machinery: strict resolution and per-attempt permission
  filtering for alternates (RFP-0001).
- **Honor the judge's ladder in streaming** — a downgraded stream must use
  `cheaper_fallbacks`, closing the gap `chat.rs:450-452` defers to this RFP (§5.2).
- **Bound each attempt in time** so fallback actually fires when a provider
  stalls, not just when it cleanly errors (§5.4).
- Keep router-owned pre-dispatch verdicts out of the upstream fallback loop (§5.3).
- Preserve attribution: the span/usage/headers/debug-log must reflect the
  connection that actually served the stream (which may be an alternate), while
  keeping today's up-front stamp so a total failure is still attributed (§5.6).

**Non-goals**
- **Mid-stream failover.** Once bytes are flowing, a later upstream error is
  delivered as an in-band SSE error event (today's behavior, `chat.rs:813-826`);
  we do not restart the stream on another provider.
- Buffering the whole response to "make streaming safe." We only ever hold back
  the connection-open, never response content.
- A global request timeout on the shared `reqwest::Client`. §5.4 is deliberately
  scoped to the stream-*open*; bounding the body would break long generations.

## 5. Proposed design

### 5.1 A streaming analogue of `try_with_fallbacks`

Factor the alternates loop so streaming shares it. Note the shape of the
existing contract (`chat.rs:110-118`): `try_with_fallbacks` does **not** resolve
or gate the primary. The caller resolves it leniently, gates it (a denial is a
terminal 403, never a fallback trigger), lets the judge possibly replace it, and
passes the winner in as an already-vetted `ResolvedRoute`. The loop only owns the
*alternates*. The shared abstraction is therefore:

> dispatch an already-vetted primary; on a failed dispatch, for each alternate:
> strict-resolve → permission-gate → dispatch; stop at the first success.

parameterized over the dispatch function. Concretely, generalize over
`F: Fn(Arc<dyn BackendProvider>, ChatCompletionRequest) -> Fut` with
`Fut: Future<Output = Result<T, BackendError>>`, instantiated at
`T = ChatCompletionResponse` (via `create_chat_completion`) and
`T = SseByteStream` (via `create_chat_completion_stream`). The return stays
`Result<(T, Option<String>, RouteResolution), BackendError>` so the caller gets
the winning resolution without re-resolving.

For streaming, `T = SseByteStream`: the first candidate to return `Ok` wins, and
only then do we commit the 200 and pipe. If every candidate fails, return the
last error as a real non-2xx — exactly what `handle_streaming` does today for
the single-candidate case.

Because status is checked at open time, this is **retry-before-first-byte** by
construction: we never pipe partial bytes from a failed attempt.

### 5.2 Where the fallback list comes from

This is the part most easily got wrong, and it is a cost and policy hole rather
than a cosmetic one. Streaming must mirror `handle_non_streaming`
(`chat.rs:926-947`) exactly:

- **Judge downgraded the request** → the fallback list is
  `downgrade.cheaper_fallbacks`, **not** `policy.fallbacks_for(...)`. Per
  `src/judge/route.rs:47-52`, those rungs are strictly cheaper than the served
  rung, so "a fallback can neither re-upgrade above the served rung nor escape
  the ladder."
- **No downgrade** → `state.fallback_policy.get()`, failing open to
  `FallbackPolicy::empty()` on a read error (serving without fallbacks beats
  failing the request), then `policy.fallbacks_for(&original_model)`.

Reading the configured policy after a downgrade would let a judge-downgraded
stream fail *upward* into the expensive model the judge just declined —
silently, and only on the failure path. This must be a test, not a comment.

### 5.3 Which errors advance to the next candidate

HTTP status alone cannot tell whether another model could serve the request. A
provider may report a missing or stale upstream model as `400`, while another
provider may use `404`; context, media, and parameter support can likewise differ
between models. Treating selected 4xx statuses as universally terminal therefore
turns recoverable provider/model mismatches into hard failures.

**Rule:** once a request has been dispatched, every returned failure may advance
an explicitly configured cross-model fallback chain, regardless of HTTP status.
With no configured fallback, the first failure returns immediately. If all
candidates fail, return the last genuine dispatch error and its headers.

The terminal boundary is instead the **origin** of the error:

- **Before dispatch:** request parsing, primary route resolution, and primary
  permission checks are router verdicts and return directly. An alternate that
  fails strict resolution, permission, capability, or lossless-representation
  checks is skipped without dispatch.
- **After dispatch:** any provider error can advance the explicit chain. This
  includes `400`, `413`, `414`, `415`, `422`, and `431`, as well as auth,
  entitlement, rate-limit, timeout, transport, and server failures.

This matches OpenRouter's any-error model fallback and LiteLLM's separation of
same-model retries from cross-model fallback: a status may be inappropriate for
an automatic retry against the same deployment while still being a valid reason
to try a different explicitly configured model.

**Skipped candidates are not dispatch failures — and must not become the
answer.** A candidate can drop out two ways, and conflating them is a live bug:

- **Skipped** — failed strict resolution, or the key isn't permitted to reach it.
  It was never dispatched. Log it and advance *without* touching `last_err`.
- **Dispatched and failed** — this becomes `last_err` and advances to the next
  configured candidate.

If a skip overwrites `last_err`, an authorized request whose primary returned
`502` and whose only alternate the key can't reach is answered with a spurious
`403` — and a primary `429`'s `Retry-After` headers are dropped with it.
Likewise a typo in the fallback policy would replace a real provider error with
an internal config error. So `last_err` is seeded from the primary's dispatch
failure and only ever replaced by another *dispatch* failure.

The same rule applies to streaming and non-streaming dispatch. The only streaming
boundary remains commit: an error can trigger fallback only before the downstream
`200` and first byte.

### 5.4 Per-attempt stream-open deadline

Fallback exists for provider incidents, and the common shape of an incident is a
**stall**, not a clean `503`. With no timeout anywhere in the client
(`src/main.rs:214`), a stalled primary hangs the request until the OS abandons
the socket, and the alternates are never reached. Without this section, RFP-0002
ships a feature that does not fire in its own headline scenario.

Wrap each candidate's dispatch in `tokio::time::timeout`:

```rust
match tokio::time::timeout(*STREAM_OPEN_TIMEOUT, open(backend, req)).await {
    Ok(inner)    => inner,                                  // Ok(stream) | Err(BackendError)
    Err(_elapsed) => Err(BackendError::api_status(504, "upstream stream-open timed out")),
}
```

- **Bounds the open only.** The timeout future is dropped the moment
  `create_chat_completion_stream` returns; the body streams unbounded, so long
  generations are unaffected. This is why it is per-attempt and not a client-wide
  `reqwest` timeout.
- **Synthesizing a `504`** lets a configured chain advance with no special case
  and gives the caller an honest status if every candidate times out.
- **Config placement.** `AppState` carries no config (`src/lib.rs`); the
  boot-time `Config` in `src/config.rs` is threaded through constructors and is
  not reachable from a handler. The established pattern for a handler-level
  tunable is a module-level `LazyLock` read from env once on first use — see
  `PROVIDER_PROBE_TIMEOUT` (`src/service/provider_connections.rs:46`) and
  `content_logging_enabled` (`src/handlers/mod.rs:53`). Follow it:

  ```rust
  // src/handlers/chat.rs
  static STREAM_OPEN_TIMEOUT: LazyLock<Duration> = LazyLock::new(|| {
      Duration::from_millis(env_parse("ROUTER_STREAM_OPEN_TIMEOUT_MS").unwrap_or(10_000))
  });
  ```

  `ROUTER_`-prefixed per the namespacing in `cd2ceb4`. Ten seconds is well above
  a healthy headers-response from every provider we route to, and well below any
  client's patience for a hung stream.
- **Streaming only.** An earlier draft of this section said to apply the same
  bound to the non-streaming dispatch "for the same reason and from the same
  value." That was wrong, and implementation caught it: `create_chat_completion`
  resolves only after the *entire generation*, so a 10s bound there would abort
  perfectly healthy long completions. The two are not symmetric — streaming's open
  is a headers-only round-trip. A stalled non-streaming primary is a real problem
  but needs a different mechanism (a much larger ceiling, or an idle-read bound);
  it is left out of scope here rather than shipped as a regression.

### 5.5 The one subtlety: providers that 200-then-fail

The window is only as safe as the backend's open-time status check. As tabulated
in §3, every current adapter checks status before returning the stream, so a
normal upstream error is a pre-open `Err` and can advance the configured chain.
A provider that returns `200 OK` and *then* emits an error as its first SSE event
is already past our commit point — that failure stays in-band (non-goal). This is
acceptable: it matches the non-streaming contract's boundary. We should **not**
"peek the first chunk" to widen the window — that reintroduces buffering/latency
and blurs the mid-stream line we deliberately drew.

### 5.6 Attribution & usage tap

The span is stamped from the primary resolution **before** the gate and dispatch
(`chat.rs:411`), deliberately — `chat.rs:40-43` documents why: a denied or
errored span must still carry system/model/connection. That up-front stamp
**stays**. What changes is that the winner *re-stamps* it, mirroring
`handle_non_streaming` (`chat.rs:963`):

- Keep the up-front `set_route_span_attributes` from the primary (and the judge's
  re-stamp on downgrade, `chat.rs:459`). An all-candidates-failed stream must
  still be attributed — that is precisely the span worth having.
- On a win, re-stamp `set_route_span_attributes` from the **winning**
  `RouteResolution`.
- Build `SseSpanTap` from the winner. It already closes over owned
  `provider_kind` / `upstream_model_id` (`chat.rs:614-637`) and is already
  constructed inside the `Ok` arm, so this falls out naturally once the loop
  returns the winning resolution.

Everything keyed on "what actually served" must follow the same
`used_model = fallback_model.unwrap_or(served_model)` rule the non-streaming path
uses, not just the one header the earlier draft named:

| surface | today (streaming) | after |
|---|---|---|
| `x-fallback-model` | not set | set when a fallback won (`chat.rs:1031`) |
| `x-chalk-routed-model` | judge downgrade only (`chat.rs:511`) | set when `used_model != original_model` |
| `chalk.router.requested_model` / `.served_model` | not set | set when `used_model != original_model` (`chat.rs:967-969`) |
| `DebugCompletionEntry.model` | `served_model` (`chat.rs:491`) | `used_model` |

### 5.7 What does *not* change

- Embeddings/images/audio/moderation stay fallback-free.
- The Anthropic-native ingress `/v1/messages` (`src/handlers/anthropic.rs`) is a
  native-wire passthrough with no fallback list. It resolves its model once —
  including provider prefixes and published aliases — and requires the selected
  connection to be Anthropic before forwarding the provider-native model ID.
- Strict resolution and permission semantics are reused, not re-specified.
- The mid-stream error path (in-band SSE error event after 200) is untouched.

## 6. Risks & cost

- **Latency of a failing primary (streaming):** each failed open costs at most one
  `ROUTER_STREAM_OPEN_TIMEOUT_MS` (§5.4), so worst-case added latency is bounded
  in *time* by `(1 + alternates) × timeout` — not merely by the count of
  alternates, which was the earlier draft's claim and was only true for a
  promptly-erroring upstream. **Non-streaming remains unbounded in time** per
  §5.4, since the deadline cannot apply there; its worst case is still a count
  bound.
- **Explicit fallback cost (§5.3).** A request defect shared by every configured
  model may now spend one attempt per candidate. The chain is finite and
  administrator-authored; matching established cross-model fallback semantics is
  worth that bounded latency.
- **New config surface.** `ROUTER_STREAM_OPEN_TIMEOUT_MS`. Set too low it
  amputates healthy slow-start providers; the 10s default is chosen with a wide
  margin, and it bounds only the open. Still schema-, migration- and proto-free.
- **Backend contract dependency:** correctness hinges on backends checking status
  at open. Per §3 this holds for *every* adapter today, since all providers
  funnel through `OpenAICompatibleBackend` or `AnthropicBackend`. The risk is forward-
  looking: any new streaming adapter must preserve "error before first byte ⇒
  `Err` from stream-open" or it silently opts out of streaming fallback
  (degrading to no-failover, which is safe). Worth a doc-comment on the trait
  method rather than tribal knowledge.
- **Per-attempt request clone.** `create_chat_completion_stream` takes the
  request by value, so each candidate needs a `req.clone()`, as
  `try_with_fallbacks` already does. For long conversations with inline image
  parts that is a non-trivial allocation per attempt. Acceptable at current
  scale; if it bites, the dispatch fn can take `&ChatCompletionRequest` and let
  adapters clone only what they mutate.

**Test shape:**
1. Mock upstream fails the first attempt's stream-open and succeeds the second —
   assert `x-fallback-model`, and that no partial bytes leaked from attempt 1.
2. Judge-downgraded stream + configured fallback list — assert the alternates
   tried come from `cheaper_fallbacks`, and that no candidate above the served
   rung is ever dispatched (§5.2).
3. Every candidate fails — assert the span still carries `llm.system` /
   `llm.model_name` from the primary (§5.6), and the response is a real non-2xx.
4. Primary returns each formerly terminal upstream status (`400`, `413`, `414`,
   `415`, `422`, `431`) — assert the alternate is reached and wins on both paths
   (§5.3).
5. A router-owned pre-dispatch denial dispatches zero candidates (§5.3).
6. Permission-denied alternate — assert the loop *skips* it and continues to the
   next candidate rather than terminating (§5.3).
7. Primary dispatch failure whose **only** alternate is skipped (denied, and
   separately unresolvable) — assert the client gets the primary's `502`, not a
   synthesized `403` or the resolver's error; and with a `429` primary, assert
   `Retry-After` still reaches the caller (§5.3).
8. A skipped alternate followed by a good one — assert skipping neither records an
   error nor stops the loop (§5.3).
9. Primary stream-open stalls past the deadline — assert the alternate is reached
   and wins (§5.4).

## 7. Rollout state

All phases below are implemented:

1. Add the `STREAM_OPEN_TIMEOUT` `LazyLock` in `src/handlers/routing.rs` and wrap
   streaming dispatch in the per-attempt deadline (§5.4). Independently
   shippable and independently valuable.
2. Keep pre-dispatch router verdicts and skipped candidates outside
   `try_with_fallbacks`' dispatch-error state; every genuine dispatch failure may
   advance the explicit chain (§5.3).
3. Generalize the alternates loop over the dispatch fn; make `handle_streaming`
   drive it with an `open_stream` closure per candidate (§5.1).
4. Wire the fallback source through the judge branch in `handle_streaming`
   (§5.2), removing the deferral comment at `chat.rs:450-452`.
5. Move attribution to re-stamp-from-winner and update the four surfaces in the
   §5.6 table.
6. Add the remaining tests and update RFP-0001's fallback policy to cover
   streaming failover before first byte.

No schema, migration, or proto changes. One new environment variable.
