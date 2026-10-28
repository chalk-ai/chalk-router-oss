---
title: First request
---

Use an existing router instance with a configured provider connection or model route. The examples use synthetic names; replace them with IDs returned by your instance. A router API key is distinct from a provider's API key.

## Set your client environment

```sh
export ROUTER_URL='http://127.0.0.1:4001'
export ROUTER_API_KEY='ck-replace-with-your-router-key'
```

This loopback URL is illustrative. Use your deployment's HTTPS URL when connecting over a network. These commands configure the client; they do not start or configure the server.

## Discover accessible models

```sh
curl --fail-with-body "$ROUTER_URL/v1/models" \
  -H "Authorization: Bearer $ROUTER_API_KEY"
```

Choose an `id` from `data`. Inspect `capabilities` to determine available protocols, modalities, and delivery modes. Restricted keys see only accessible models. The list is not paginated.

## Send chat

```sh
curl --fail-with-body "$ROUTER_URL/v1/chat/completions" \
  -H "Authorization: Bearer $ROUTER_API_KEY" \
  -H 'Content-Type: application/json' \
  --data '{"model":"chalk/example-chat","messages":[{"role":"user","content":"Explain a cache in one sentence."}]}'
```

The response's `choices[].message` contains assistant content or tool calls. Token usage may include provider-specific detail. See the [validated chat request and illustrative response](/reference/examples/).

For SSE, add `"stream":true` and use `curl -N`; a successful HTTP status alone does not establish stream completion. [Streaming and errors](/guides/errors-streaming/) explains terminal events and partial failures.

## Common first-request failures

| Result | Check |
| --- | --- |
| 401 | Send a valid router credential using the exact `Bearer ` prefix. |
| 403 | Check model/route permissions and selected environment. |
| 404 / `model_not_found` | Use a configured ID visible to this key. |
| 400 capability/conversion error | Select a model supporting the requested protocol and features. |
| 429 | Inspect quota headers and `retry-after`. |
| 501 | Check whether the operation or requested feature is implemented. |

Admin clients use the separate [gRPC guide](/guides/admin/). The documentation preview requires no router, database, or provider credential: run `just docs-install`, then `just docs-dev` from the repository root.
