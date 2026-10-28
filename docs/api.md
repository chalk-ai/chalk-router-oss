# API reference

[Documentation index](README.md) · [Repository overview](../README.md)

Requests require the credentials described in [Authentication](../README.md#authentication).
Provider support is determined by each model's advertised capabilities. The
route registration in [main.rs](../src/main.rs) and the
[handler pipeline table](../src/handlers/mod.rs) define the implemented surfaces.

## Proxied LLM API (implemented)

These forward to the resolved upstream provider (see [Model routing](model-routing.md)):

- `POST /v1/chat/completions` — chat completions, incl. streaming, tool calls,
  and structured output
- `POST /v1/responses` — OpenAI Responses API, including lossless native
  streaming for OpenAI and validated chat-compatible provider fallback
- `POST /v1/responses/compact` — native OpenAI remote conversation compaction
- `GET /v1/responses/:response_id`, `DELETE /v1/responses/:response_id`,
  `POST /v1/responses/:response_id/cancel` — lifecycle operations on connections
  that support them
- `POST /v1/messages`, `POST /v1/messages/count_tokens` — Anthropic Messages
  API pass-through (qualified catalog IDs and published aliases are resolved and
  stripped before forwarding; bare IDs retain implicit Anthropic selection)
- `POST /v1beta/interactions` — native Gemini Interactions, including streaming,
  thinking settings, signed history, and tool conversations (see [Native Gemini Interactions](#native-gemini-interactions))
- `POST /v1/embeddings` — text/image embeddings
- `POST /v1/images/generations` — image generation
- `GET /v1/models`, `GET /v1/models/:id` — aggregated model listing across
  configured providers
- `POST /v1/audio/speech`, `POST /v1/audio/transcriptions`,
  `POST /v1/audio/translations` — audio
- `POST /v1/moderations` — text moderation with request-rate/concurrency quotas;
  successful calls record zero tokens, and token policies do not apply
- `POST /v1/alpha/decisions`, `POST /v1/decisions`, `POST /v1/systemone` —
  Router Decisions, OpenAI Decisions, and TypeSafe-compatible requests
- `POST /v1/traces/spans` — OpenInference span ingest (see [Tracing](tracing.md))

## Native Gemini Interactions

Use a saved Gemini connection prefix or a published router model alias in `model`.
The router resolves that model, authenticates and authorizes the call, and applies
quotas and usage accounting before forwarding to Google's native Interactions API.
Only the top-level model is rewritten. Native content, tool calls/results, thought
signatures, unknown fields, and streaming event IDs are preserved. Google validates
the model-specific features. Signatures are opaque and redacted only from telemetry.

```sh
curl http://localhost:4001/v1beta/interactions \
  -H "x-goog-api-key: $ROUTER_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"gemini/gemini-2.5-flash","input":"Hello","store":false}'
```

The example assumes the saved connection has prefix `gemini/`. The presented key
is a **router API key**; the router uses that connection's stored Google credential
upstream. `Authorization: Bearer` also works. Set `stream: true` for native SSE.

For stateless tool conversations, send the returned steps back unchanged, in their
original order, followed by the corresponding `function_result` steps. Keep using
the same explicit connection prefix and model for signed history or
`previous_interaction_id` continuations. The router does not store conversations
or rewrite interaction IDs. Google's own storage default is preserved; use
`store: false` to opt out. Fallback is allowed only for fresh requests with
`store: false`, and only to eligible Gemini connections. Signed history, tool
continuations, and stored interactions never fall back.

Token reservations estimate visible inline input plus an output allowance.
Referenced history (`previous_interaction_id` or cached content) is not fetched,
so requests with those references are rejected when a matching token policy
requires a bound. Supply the full supported history inline instead. Requests
with only request-rate or concurrency policies can use referenced history.
Opaque or unsupported media can also make the token bound unknown. Final usage
accounting uses Google's reported usage.

The playground uses this native route for chat models advertising Gemini
Interactions support. It streams answer text and retains the complete native
steps in the page for stateless follow-up requests. Clearing the conversation,
changing models, or reloading the page discards that history. The playground
does not execute tools; tool conversations still require an API client.

The Images playground also uses native Interactions for models advertising that
interface, including image-only models whose native support is still marked
unknown. Requests are non-streaming with `store: false`; returned images and
accompanying text are displayed together. Select an image-generation model such
as `gemini/models/gemini-3.1-flash-image` (using your connection's prefix).
Models without an advertised native interface retain `/v1/images/generations`.

This route supports synchronous model interactions, with or without streaming.
Agent/background jobs and interaction retrieval, deletion, and cancellation are
not yet supported and return 501. Native usage must be reported by Google; the
router does not guess token counts for opaque reasoning or media.

OpenAI-format clients continue to use Google's OpenAI-compatible endpoint for
`/v1/chat/completions`. `/v1/responses` retains the existing stateless Chat
translation and buffered streaming behavior. Those interfaces expose the
compatibility subset, not Gemini's full native protocol.

## OpenAI Responses

Native Responses streams use `ROUTER_STREAM_OPEN_TIMEOUT_MS` (default 10,000)
only while opening upstream response headers. Chat-compatible providers must
finish a non-streaming Chat Completions generation before the router can
synthesize Responses events, so those attempts instead use the separate
`ROUTER_RESPONSES_COMPAT_TIMEOUT_MS` total deadline (default 300,000). This
keeps stalled compatibility attempts eligible for fallback without imposing a
10-second limit on healthy generations.

Translated Responses map `reasoning.effort` to Chat's `reasoning_effort`; empty
reasoning objects are a no-op. Direct Claude and Bedrock Mantle routes map
`minimal` to `low`, use disabled thinking for `none`, and otherwise request
adaptive thinking with the supplied effort. Omission preserves provider defaults.
Upstream validates support, so new models and effort strings need no declarations.
Recognized HTTP 400 rejections of those reasoning controls can use configured
fallbacks before a response opens, with the original controls intact. Invalid
values, unrelated errors, and unknown diagnostic wording remain terminal.
The router does not invent manual token budgets. Bedrock Runtime Converse rejects
explicit reasoning controls before dispatch, allowing a compatible fallback.
`prompt_cache_key` and string-valued `client_metadata` are accepted client hints
but are not forwarded to Chat providers; this does not enable upstream caching.
Native Responses requests retain these fields unchanged. Reasoning summaries
remain unsupported on translated routes.

### Background Responses

Background OpenAI Responses are tracked automatically on explicitly supported
native/adapted connections with retrieval support. The router persists the
creator, winning connection, response ID, normalized terminal usage, and pricing
revision; it does not persist prompts, outputs, or credentials. A background
worker retrieves through the original connection independently of client
disconnects. Retrieval and resumed streams do not write duplicate usage events.
Postgres is required for restart-safe, multi-replica accounting; without a
database the job store is in-process and volatile. Existing `ROUTER_USAGE_BACKEND`
settings still control durable usage reporting. Matching token budgets, RPM,
TPM, and concurrency limits are enforced at admission. Background grants belong
to the job, not the HTTP response, so disconnects and ordinary lease expiry do
not release them. Reported terminal usage reconciles tokens; terminal responses
without usage retain the reservation and release concurrency. Unresolved
execution retains both until it can be reconciled. Lifecycle requests consume
request-rate quota only, so generation concurrency does not prevent polling or
cancellation. Restart-safe enforcement requires `ROUTER_QUOTA_BACKEND=postgres`
in addition to Postgres accounting; memory quota remains per-process and volatile.
Admission fails closed if durable ownership cannot be recorded.

Background token policies require a conservative bound: the visible inline
input plus `max_output_tokens` or a known model output limit. Stored history and
hosted tools have unknown token bounds and are rejected when token policies
match, but remain usable under request-rate/concurrency-only policies. The
reservation fold includes every eligible fallback, including unknown bounds.
Fallback is allowed only before acceptance for explicit upstream rejections;
ambiguous transport failures never regenerate on another provider. Accepted
jobs and lifecycle operations always use the original connection.

Polling is always enabled. `ROUTER_BACKGROUND_RESPONSES_POLL_INTERVAL_SECS`
(default 30), `ROUTER_BACKGROUND_RESPONSES_POLL_BATCH_SIZE` (default 100), and
`ROUTER_BACKGROUND_RESPONSES_POLL_CONCURRENCY` (default 4) bound reconciliation.
Failed polling backs off rather than regenerating through a fallback. An
unresolved submission or missing upstream response is not assumed to cost zero;
inspect `chalk_router.background_response_jobs` for its safe error classification.
Responses created before this rollout are not retroactively enrolled. Token/cost
telemetry uses the existing OpenInference/Braintrust span metrics, independently
of the durable accounting ledger and trace sampling.

Authenticated admin gRPC provides bounded, metadata-only background-job listing.
Settled metadata is retained for 30 days by
default (`ROUTER_BACKGROUND_RESPONSES_RETENTION_DAYS=0` disables cleanup only);
usage history, quotas, non-settled jobs, and upstream responses are preserved.
See the [background Responses operations runbook](background-responses-operations.md)
for inspection, automatic polling/accounting behavior, and retention details.

## Admin / management surface

Exposed over gRPC + gRPC-Web (`chalk.router.v1.*`) only. Covers API keys,
usage + usage histograms, usage pools, rate limits, providers, settings, and
model routes with route-owned judge and fallback policies. Plus:

- `GET /health`, `GET /v1/healthcheck` — legacy HTTP health checks
- `grpc.health.v1.Health/Check` — native gRPC startup/readiness probes

Large gRPC admin collections support opaque cursor pagination. An omitted
`limit` returns 40 rows; explicit limits are clamped to 1,000. Pass each
response's `next_cursor` into the next request. Background-job pages have a
separate cap of 100. `ListAllUsage` retains its legacy exhaustive response.

## Stubbed / partial

These vendor API surfaces are incomplete. Unsupported operations return
`501 Not Implemented`; missing-resource handlers may return `404`:

- Assistants (`/v1/assistants…`)
- Files (`/v1/files…`) — upload disabled; no supported file lifecycle
- Fine-tuning (`/v1/fine_tuning/jobs…`)
- Batch (`/v1/batches…`)
- `POST /v1/images/edits`, `POST /v1/images/variations` → `501`

HTTP model deletion returns 501 without mutation; use the authorized ModelRouteService/DeleteModelRoute admin RPC.
