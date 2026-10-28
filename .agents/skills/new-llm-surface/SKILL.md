---
name: new-llm-surface
description: Checklist for adding or changing an LLM request handler in chalk-router (chat, messages, responses, embeddings, images, audio, or a new surface) so it goes through the same pipeline stages as its siblings and is tested the same way. Use before writing or reviewing anything under src/handlers/ that dispatches to a provider.
---

# new-llm-surface

Three LLM surfaces (`/v1/chat/completions`, `/v1/messages`, `/v1/responses`)
were built as three pipelines and drifted; the audit that fixed it left a
surface × stage table at the top of `src/handlers/mod.rs`. Read that table
first. A new or changed handler must either use the stage helper below or add
a row to the table saying why the stage is deliberately skipped.

## Stages and their helpers (in request order)

| Stage | Helper | Notes |
|---|---|---|
| Auth | middleware (`src/middleware.rs`) | The resolved key arrives as `ResolvedApiKey` in request extensions. |
| Resolve + stamp | `state.backend.resolve*`, then `LlmSpanWriter` request attrs | Stamp request attributes **before** the permission gate so denied traffic is visible in traces. |
| Permission | `check_permission` / `forbidden_response` | Against the resolved connection, never the public model string alone. |
| Capability | `run_passthrough` (non-chat) or the requirements built in `capability_requirements.rs` | Enforce in the handler layer only; `RouterBackend` no longer checks. |
| Prompt cache | `state.chat_cache_context` (typed bodies) or `state.raw_body_cache_context` (byte passthrough) | Cache hits still acquire request quota with `TokenReservation::NotApplicable`. |
| Judge | `crate::judge::route` | Chat only; needs the OpenAI chat shape. |
| Quota + fallback | `try_with_fallbacks_if` | One reservation for the whole request (the fold is conservative on unknown routes; do not change it); per-candidate rate limit inside `dispatch_attempt`. Native passthrough must turn upstream 5xx/429 into `Err` so the walk continues, and forward the last upstream body unchanged when every candidate fails. |
| Settlement | `handlers/usage.rs::with_quota_settlement` | Streaming and non-streaming both go through it; never settle inline, never use `finish_failed()` for partial streams. |
| Streaming | `sse.rs::{SseDecoder, encode_frame, done_frame}` | Error frames built from `types::ErrorResponse::new`; per-protocol envelope only. |
| Trace | `trace_attributes.rs` / `LlmSpanWriter` | No direct `set_attribute`. |
| Debug ring | `state.debug_log` | Chat only today. |
| Unsupported | `handlers::not_implemented(feature)` | 501, never a fabricated success or a bare 404. |

## Checklist for a change

1. Which rows of the table change? Update `src/handlers/mod.rs` in the same commit.
2. Does the handler take `State`? A handler without it bypasses permission and quota.
3. Streaming and non-streaming produce the same usage numbers (including
   cached-prompt token fields for Anthropic) and the same cache size ceiling.
4. Anthropic ↔ OpenAI translation (`src/translation/anthropic_chat.rs`): a dropped
   field is either mapped or listed in the module doc with a `debug!` log.
5. Tests to add or extend under `tests/integration/` (fixtures in `support/`):
   - primary connection returns 503 → fallback serves (streaming and non-streaming);
   - 503 with no fallback → client receives the upstream body unchanged;
   - 4xx from upstream → no fallback attempt;
   - usage recorded exactly once through `with_quota_settlement`;
   - permission denial before any upstream call;
   - streaming frame bytes for the surface's error event;
   - capability rejection for an unsupported operation.
6. Changelog bullet under `changelogs/YYYY-MM-DD/` if a client can notice.
7. Run the `/router-gate` skill.
