---
title: Anthropic and Gemini APIs
---

## Anthropic Messages

`POST /v1/messages` accepts an Anthropic-shaped JSON body. Native/adapted dispatch retains unknown provider fields and rewrites the model for the selected upstream. `anthropic-*` headers are forwarded; router credentials are not forwarded as provider credentials. The provider's version/beta headers determine its feature semantics.

```sh
curl --fail-with-body "$ROUTER_URL/v1/messages" \
  -H "Authorization: Bearer $ROUTER_API_KEY" \
  -H 'Content-Type: application/json' \
  -H 'anthropic-version: 2023-06-01' \
  --data '{"model":"example-connection/example-claude","max_tokens":128,"messages":[{"role":"user","content":"Hello"}]}'
```

The familiar `model`, `messages`, and `max_tokens` fields are a useful minimal native request. The router does not define a closed Anthropic schema: provider-specific thinking, tools, cache controls, content blocks, and future fields are subject to native capabilities. A Chat translation accepts only representable content/tool/control features and rejects unsupported semantics. Native replies/errors can retain Anthropic shapes; translated replies are encoded as Anthropic messages/events.

`POST /v1/messages/count_tokens` uses the native token-count operation. Its provider response commonly contains `input_tokens`, but the router does not normalize it into Chat usage. Counting does not perform fallback or billable token settlement. A missing string model currently produces a plain-text 400, one reason not to assume every error is JSON.

For `stream:true`, parse Anthropic named SSE events such as message/content-block starts, deltas, and message stop. Do not use Chat's `[DONE]` termination rule.

## Gemini Interactions

`POST /v1beta/interactions` is a native Gemini surface. A normal model request requires a nonempty string `model` naming a routed model or connection prefix. If present, `stream`, `store`, and `background` must be booleans. The raw JSON representation retains unknown fields, signatures, tool steps, and nested data.

```sh
curl --fail-with-body "$ROUTER_URL/v1beta/interactions" \
  -H "Authorization: Bearer $ROUTER_API_KEY" \
  -H 'Content-Type: application/json' \
  --data '{"model":"example-connection/example-gemini","input":"Hello","store":false}'
```

Native JSON and SSE bodies remain provider-shaped. Agent and background interactions return 501. GET/DELETE `/v1beta/interactions/{id}` and POST `/{id}/cancel` are mounted 501 placeholders.

Fallback is allowed only with explicit `store:false` and no bound history. Stored turns, prior-interaction IDs, signatures, cached content, or tool history must remain on the selected connection. Omitted `store` does not mean stateless: Google's default is true. There is no router-provided collection pagination for interactions.

These native examples are illustrative provider payloads, not fixtures validated against a provider service. The router has no complete Anthropic or Gemini JSON Schema to generate; this is an explicit reference gap, not an invitation to substitute another API's schema.

Sources: [Anthropic handler](https://github.com/chalk-ai/chalk-router/blob/main/src/handlers/anthropic.rs), [Anthropic translation tests](https://github.com/chalk-ai/chalk-router/blob/main/tests/integration/anthropic_translation_test.rs), [Gemini handler](https://github.com/chalk-ai/chalk-router/blob/main/src/handlers/gemini.rs), [Gemini tests](https://github.com/chalk-ai/chalk-router/blob/main/tests/integration/gemini_interactions_test.rs).
