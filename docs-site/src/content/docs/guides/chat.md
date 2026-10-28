---
title: Chat completions
---

`POST /v1/chat/completions` accepts JSON and returns a chat response or SSE stream. Choose a model supporting the requested input, tools, output format, and protocol.

## Request and response contracts

[ChatCompletionRequest](/reference/http-types/#chatcompletionrequest) documents the router's typed view: `model`, `messages`, optional sampling/token controls, tools, output format, stream options, reasoning effort, and router `fallbacks`. [ChatMessage](/reference/http-types/#chatmessage), [Tool](/reference/http-types/#tool), and linked types define nested fields. [ChatCompletionResponse](/reference/http-types/#chatcompletionresponse) and [ChatCompletionChunk](/reference/http-types/#chatcompletionchunk) describe the response and chunk views.

The native envelope validates that `model` is a string, `messages` is an array, and `stream` is boolean or null. It retains fields beyond the typed view rather than imposing a closed schema. Native provider semantics still govern roles, message content, tool behavior, and parameter ranges.

Translated routes parse a stricter representation. Unsupported semantic fields fail instead of silently changing the request. Shared translated controls reject `n` other than 1, nonzero presence/frequency penalties, and nonempty `logit_bias`. Restrictions vary by backend; for example Converse cannot represent `reasoning_effort`, or disabled parallel tool calls when tools are present.

## Streaming

Set `stream:true`; optionally set `stream_options:{"include_usage":true}`. Parse complete SSE frames, accumulating text and tool-call deltas by their indexes. JSON fragments inside tool arguments are not necessarily individually complete. Chat uses a terminal `data: [DONE]` frame. An error can arrive after HTTP headers; preserve partial results and treat the request as failed when the stream reports an error or terminates unexpectedly.

[Validated examples](/reference/examples/#streaming-chat-with-usage) include both JSON request forms. Usage is requested from native upstreams for accounting; the router suppresses unsolicited client usage chunks when the client did not request them.

## Router fallbacks and diagnostics

`fallbacks` is a router extension with shape `map<string, string[]>`: concrete `connection_id/upstream_model_id` targets mapped to alternative concrete targets. Route aliases are not valid fallback targets. The router removes this field before provider dispatch. Model routes can also own fallback policies; per-request overrides are subject to routing and permission validation.

Fallback attempts are capability-checked and share one quota reservation. Do not infer that every error is retried or that a partially emitted stream can safely be replayed. Headers such as `x-chalk-routed-model` and `x-fallback-model` can report the selected route; they are conditional diagnostics, not mandatory response fields.

Sources: [native envelope](https://github.com/chalk-ai/chalk-router/blob/main/src/native_chat.rs), [Chat handler](https://github.com/chalk-ai/chalk-router/blob/main/src/handlers/chat.rs), [translated controls](https://github.com/chalk-ai/chalk-router/blob/main/src/translation/chat_controls.rs), [native fidelity tests](https://github.com/chalk-ai/chalk-router/blob/main/tests/integration/native_fidelity_test.rs).
