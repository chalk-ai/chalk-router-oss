# RFP-0003: API-Key Permission Scope for Non-LLM Endpoints (Audio + Moderation)

> **Reading note (2026-10-26):** This proposal's deferred-audio and synthetic
> moderation baseline is historical. Audio and text moderation now use the
> routed permission/capability pipeline; moderation dispatches upstream with
> request-only quotas. See the [handler pipeline table](../src/handlers/mod.rs)
> and [API reference](../docs/api.md). The original scope and rationale remain
> below as a design record.

- **Status:** Partially implemented — images gated; audio permission scoping deferred
- **Created:** 2026-07-23
- **Depends on:** [RFP-0001](0001-router-motivation-and-architecture.md) — §API-key permissions
- **Supersedes:** `docs/design/permission-scope-non-llm-endpoints.md`
- **Scope:** Whether (and how) to extend `ApiKeyPermission` enforcement beyond
  Chat Completions, Responses, Anthropic ingress, embeddings, and images to audio
  (speech / transcription / translation). Moderation is reconsidered when it
  becomes a real upstream dispatch.

---

## 1. Summary

`ApiKeyPermission` fences Chat Completions, Responses, Anthropic ingress,
embeddings, and images to a key's `provider`/`connection_id`/`models` allowlist.
Audio resolves its requested model through the normal router but its handlers do
not apply the permission predicate. Closing that gap is deferred: it is a
behavior change for existing audio users and audio is not currently important
enough to justify the rollout. No schema or proto change would be required when
the work resumes.

Moderation currently returns a local synthetic response and does not dispatch to
a provider. Permission scoping for moderation is therefore out of scope until a
real upstream implementation exists.

## 2. Current state (as built)

`ApiKeyPermission::allows(provider_kind, connection_id, public_model_id)`
(`src/api_keys.rs`) is enforced on the routed LLM paths and images. Its
axes are `provider` (matched against the provider **kind** only), `connection_id`
(the resolved connection's **immutable id**), and `models`:

- **Chat Completions and Responses** — primary gated before dispatch (terminal
  403), each fallback candidate filtered.
- **Anthropic Messages/count-tokens** — resolved connection gated before native,
  adapted, or translated dispatch.
- **embeddings** (`src/handlers/embeddings.rs`) — single primary gate.
- **images** (`src/handlers/images.rs`) — resolves the route once and gates before
  dispatch; a model-less request authorizes against the resolved OpenAI default
  connection's real identity via `resolve_default` (covered by
  `tests/integration/api_key_permission_test.rs`).

Audio does **not** consult the allowlist:

| endpoint | routing | resolvable to a connection? | allowlist checked? |
|---|---|---|---|
| `create_image` | routes on model prefix (like chat), else OpenAI | **yes** (when a model is supplied) | **yes** |
| `create_speech` | routes on requested model | **yes** | no |
| `create_transcription` | routes on requested model | **yes** | no |
| `create_translation` | routes on requested model | **yes** | no |
| `create_moderation` | local synthetic stub | no upstream connection | n/a |

Because audio is model-routed, an otherwise restricted key can currently reach
an audio-capable connection outside its allowlist. This is a real consistency
gap, but the decision for this RFP is to document and defer it.

## 3. Decision

1. **Images: implemented.** Image generation resolves once and checks all three
   permission axes before dispatch.
2. **Audio: deferred.** Keep the current behavior documented. Do not add a
   partial provider-only check or silently introduce new 403s.
3. **Moderation: defer with the endpoint.** Revisit permission semantics when
   moderation stops being a local stub and gains a real provider route.

Capabilities and permissions remain separate: an explicit audio capability says
that a route can serve the request; it does not grant the caller access to that
route.

## 4. Follow-up implementation shape

When audio permission scoping is prioritized, each audio handler should mirror
the resolve-once contract used by images:

1. Resolve the requested public model exactly once.
2. Check `permission.allows(provider_kind, connection_id, public_model_id)` on
   that resolution.
3. Apply the audio capability requirements.
4. Dispatch directly to the resolved leaf backend with the stripped model id.

Add speech, transcription, and translation cases to
`tests/integration/api_key_permission_test.rs`, including provider-, connection-,
and model-scoped keys. Ship the behavior change with a changelog entry. No
database migration or proto change is expected.
