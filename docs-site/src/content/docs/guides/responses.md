---
title: Responses and background lifecycle
---

| Method | Path | Contract |
| --- | --- | --- |
| POST | `/v1/responses` | Create; JSON or SSE |
| POST | `/v1/responses/compact` | Native/adapted compaction |
| GET | `/v1/responses/{response_id}` | Retrieve; optional `stream`, `starting_after` |
| POST | `/v1/responses/{response_id}/cancel` | Cancel on the original connection |
| DELETE | `/v1/responses/{response_id}` | Delete on the original connection |

There is no inference collection-list endpoint. Admin users can inspect tracked background jobs with [ListBackgroundResponses](/reference/grpc-background-response/#listbackgroundresponses).

## Native and translated schemas

Native Responses uses a lossless JSON envelope: provider fields, explicit nulls, and exact JSON values are retained. The router validates routing controls and consults capabilities; the provider validates its full native schema. [CreateResponseRequest](/reference/responses-types/#createresponserequest) is the compatibility codec, **not** a complete or restrictive native schema. [CreateResponseResponse](/reference/responses-types/#createresponseresponse) and linked output-item/usage types describe the router's typed view.

A translated Chat route supports a subset of text/message input, function tools, output format, and reasoning effort. It rejects `previous_response_id`, `background:true`, `store:true`, nonempty metadata, service-tier/truncation semantics, text verbosity, reasoning summaries, non-function tools, and strict tools. Known `include` selectors can be accepted but omitted; encrypted reasoning continuity is unavailable. `prompt_cache_key` and string-valued `client_metadata` can be accepted but omitted, so they do not promise upstream caching. Unknown semantic fields/selectors fail explicitly.

The [stateless example](/reference/examples/#stateless-responses-request) uses `store:false` and is tested through the compatibility conversion.

## Opaque response IDs

Use returned response IDs unchanged. The router can wrap a provider response ID in an opaque locator that pins the originating connection. Retrieve, cancel, delete, and continuation enforce the locator's original connection and the caller's current permissions. Do not decode, reconstruct, or replace these IDs with guessed upstream IDs. Lifecycle and compaction require appropriate native/adapted capabilities; they are not emulated with Chat or sent through a fallback chain.

```sh
# RESPONSE_ID is the exact opaque id returned by a previous create.
curl --fail-with-body "$ROUTER_URL/v1/responses/$RESPONSE_ID" \
  -H "Authorization: Bearer $ROUTER_API_KEY"
```

## Background work and streaming

`background:true` requires explicit background and retrieval support. Work can outlive the create HTTP connection. The router tracks quota/concurrency ownership and later usage settlement; an ambiguous submission that might have been accepted is not replayed on another connection. A definite rejection before acceptance can permit fallback.

For resuming a supported provider stream, GET accepts `stream=true` and unsigned `starting_after`. `starting_after` without streaming is invalid. This is an event-resumption cursor, not collection pagination. [RetrieveResponseQuery](/reference/response-query-types/#retrieveresponsequery) is generated from the handler.

Responses SSE uses named lifecycle events and provider-specific terminal/failure events. Inspect events, not only HTTP status, and do not apply Chat's `[DONE]` rule to every protocol. Native JSON/SSE status, content type, and bodies are preserved except required locator rewrites.

Sources: [Responses handler](https://github.com/chalk-ai/chalk-router/blob/main/src/handlers/responses.rs), [translation constraints](https://github.com/chalk-ai/chalk-router/blob/main/src/translation/responses_chat.rs), [lifecycle tests](https://github.com/chalk-ai/chalk-router/blob/main/tests/integration/responses_test.rs), [translation fidelity tests](https://github.com/chalk-ai/chalk-router/blob/main/tests/integration/responses_chat_fidelity_test.rs).
