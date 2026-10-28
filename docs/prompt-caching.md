# Prompt caching (exact prefix)

[Documentation index](README.md) · [Repository overview](../README.md)

The router can answer a request from a cache of finished completions instead of
calling a provider. The SHA-256 key covers the whole conversation prefix and
response-affecting parameters (`model`, `temperature`, `top_p`, `max_tokens`,
`stop`, `tools`, `response_format`, …), using each surface's cache identity.
There is no similarity search or prefix truncation: advancing the conversation
by a turn changes the key. Chat requests are canonicalized; Anthropic pass-through
uses the raw request body and response-shaping headers, including `anthropic-beta`.
See [retained JSON semantics](schema-json-performance.md) for schema ordering
and normalization details.

It lives above the provider backends (`src/prompt_cache/`), so it is
**provider-independent** — a `chalk/*` route that moves between OpenAI, Bedrock,
and vLLM keeps its cache — and it covers **both** request shapes:

- `/v1/chat/completions` stores the canonical non-streamed completion. `stream`
  is excluded from the digest, so a streamed and a non-streamed ask for the same
  prompt share one entry; a streaming hit is re-emitted as SSE.
- `/v1/messages` (Anthropic passthrough) stores the upstream bytes verbatim —
  a JSON body or a whole SSE stream — and replays them unchanged. Here `stream`
  *is* part of the digest, because the two shapes are different stored values.

A generation that did not finish is never stored, so a truncated answer can't be
replayed as a complete one. Cache hits do **not** record usage: no upstream call
was made, and billing the hit would double-charge the completion that populated
it.

## Per-token policy

Caching is opt-in **per API token**, on the key's `permission.cache`
(`ApiKeyPermission` / the `CachePolicy` proto message):

```jsonc
{
  "permission": {
    "cache": {
      "enabled": true,
      "scope": "team",          // "model" | "team" | "token" (default "token")
      "ttl_secs": 900,          // optional; inherits the deployment default
      "max_entry_bytes": 262144 // optional; clamped to the deployment ceiling
    }
  }
}
```

`scope` chooses how far a cached answer travels. Every scope is already
partitioned by **model** and by **Chalk environment**, so no scope can serve one
model's answer for another's, or cross a tenant boundary:

| scope | shared with | use when |
| --- | --- | --- |
| `model` | every token in the environment | all tokens are one trust domain (a shared eval harness, one product surface) |
| `team` | tokens in the same usage pool | tokens are owned per team; a key with no usage pool degrades to `token` |
| `token` | nobody (default) | tokens are handed to mutually-untrusted callers |

A hit is reported to the caller as `x-chalk-cache: hit` with `x-chalk-cache-key`
and `x-chalk-cache-age`; a miss on a cache-enabled key reports
`x-chalk-cache: miss`. A key with no policy gets no header at all. The same
decision lands on the span as `chalk.router.cache.{status,scope,backend,key,age_secs}`.

The cache is consulted **after** the key's permission gate — a hit is never a way
around the allowlist. Response caching is bypassed for routes with a `RouteGrid`
and for Chat Completions requests containing fallback overrides, so a cached
generation cannot bypass the current routing policy. The judge's classification
cache is separate from this response cache.

## Backends

The storage backend is pluggable behind the `PromptCache` trait; two ship today.

| `ROUTER_PROMPT_CACHE_BACKEND` | behaviour |
| --- | --- |
| `disabled` (default) | no caching; the request path pays one `Option` check |
| `memory` | in-process, bounded, true LRU with per-entry TTL. Per-replica and lost on restart — a single replica or local dev |
| `valkey` (alias `redis`) | shared across replicas, survives restarts. Lazy connect, 250 ms per-command deadline, errors swallowed as misses |

| env var | default | applies to |
| --- | --- | --- |
| `ROUTER_PROMPT_CACHE_URL` | — | `valkey` (required) |
| `ROUTER_PROMPT_CACHE_NAMESPACE` | `chalk-router:prompt-cache` | `valkey` key prefix |
| `ROUTER_PROMPT_CACHE_TTL_SECS` | `3600` | default entry lifetime |
| `ROUTER_PROMPT_CACHE_MAX_ENTRY_BYTES` | `1048576` | ceiling on one entry (and the clamp on a per-key value) |
| `ROUTER_PROMPT_CACHE_MAX_ENTRIES` | `10000` | `memory` |
| `ROUTER_PROMPT_CACHE_MAX_BYTES` | `268435456` | `memory` |

No backend is load-bearing: an unreachable or misconfigured cache degrades to
serving uncached rather than failing requests or refusing to boot.
