---
title: Errors, streaming, and request diagnostics
---

## HTTP errors

Router-originated errors generally use [ErrorResponse](/reference/http-types/#errorresponse):

```json
{"error":{"message":"Synthetic error example","type":"invalid_request","param":null,"code":null}}
```

Native provider errors can retain provider bodies/statuses. Framework JSON/multipart/query rejections and some placeholder 404s can be text or empty bodies. Inspect HTTP status and content type before parsing. A single JSON schema does not describe every error.

| Status | Common meaning |
| --- | --- |
| 400 | Invalid body/parameter, unsupported translation, or capability mismatch |
| 401 | Missing/invalid credential (`authentication_error`, `invalid_api_key`) |
| 403 | Auth permission failure (`permission_error`, `access_denied`) or model access failure (`access_denied`); some service preconditions also map to 403 |
| 404 | Unknown/inaccessible model/resource, unsupported placeholder retrieve, or unknown path |
| 409 | Resource identity conflict |
| 429 | Quota/rate limit (`rate_limit_exceeded`) |
| 500 | Internal failure or upstream transport failure |
| 501 | Unimplemented route/feature |
| 502 | Undecodable/unreadable upstream response, or provider-returned 502 |
| 503 | Credential/environment/usage-accounting unavailable, missing required storage, or not ready |
| 504 | Local routing deadline |

Upstream APIs may return other statuses. `model_not_found` can appear in `error.code` with `param:"model"`; unavailable usage accounting returns `error.type:"server_error"` and `error.code:"usage_accounting_unavailable"`. Do not automatically retry every error: side-effecting native operations and accepted background requests may already be executing. There is no generic router idempotency-key guarantee.

## SSE consumers

Use an SSE parser that buffers until a complete event boundary; a network chunk is not a JSON document or SSE event. Handle multiline `data`, event names, and UTF-8 boundaries. After headers are sent, a failure must be represented in the stream or by an interrupted connection, so HTTP 200 is insufficient.

| Surface | Completion handling |
| --- | --- |
| Chat | Accumulate choice/tool deltas; optional usage chunk; `data: [DONE]` |
| Responses | Named response lifecycle/terminal events; inspect failed/incomplete state |
| Anthropic | Named message/content-block events; message stop or error |
| Gemini | Native provider event stream; preserve provider semantics |
| Audio | Buffered response, even when upstream uses SSE; no incremental-delivery promise |

Never blindly replay a partially delivered generation as though no output existed. Record partial content and the failure state separately.

## Headers and usage

`x-trace-id` and `x-span-id` identify inference traces where the handler adds them. Conditional routing headers include `x-chalk-routed-model` and `x-fallback-model`; cache diagnostics include `x-chalk-cache`, `x-chalk-cache-key`, and `x-chalk-cache-age`. Quota responses can include `retry-after`, `x-chalk-error-code`, and `x-chalk-quota-*` details. Header availability varies by handler/outcome.

Provider header forwarding is allowlisted. Chat forwards `openai-beta` and `x-client-request-id`; Responses maintains a separate allowlist; Anthropic forwards its `anthropic-*` headers. An accepted browser CORS header is not a promise it is forwarded upstream. Browser origins must also be allowed by the server's configured code path.

Usage can be reported, estimated, or legacy unknown. Public list-price estimates are not invoices; missing pricing differs from zero. See [usage administration](/guides/admin/#usage-queries) and [UsageSource](/reference/grpc-api-key/#usagesource).

Sources: [error mapping](https://github.com/chalk-ai/chalk-router/blob/main/src/handlers/mod.rs), [SSE helpers](https://github.com/chalk-ai/chalk-router/blob/main/src/handlers/sse.rs), [provider forwarding](https://github.com/chalk-ai/chalk-router/blob/main/src/backend/provider.rs).
