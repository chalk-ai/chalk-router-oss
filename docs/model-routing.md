# Model routing and providers

[Documentation index](README.md) · [Repository overview](../README.md)

## Provider connections

Every upstream provider is a **connection** — an encrypted credential plus
endpoint config stored in the database (`chalk_router.provider_connections`, encrypted
at rest) and managed through `ProviderConnectionService`
(List/Create/Update/Delete/Test) and the **Providers** tab in the admin UI.
There are no env-var provider keys: no provider is enabled until a connection for
it is stored, and a connection added or rotated at runtime takes effect within a
short cache window without a restart. Every provider kind can have multiple
connections, including built-ins such as `openai`, `anthropic`, `gemini`, and
`vertex`. Connection names and non-null prefixes are unique within an
environment. A built-in connection may use its kind's canonical prefix
(`openai/`, `anthropic/`, …) or a custom prefix; an `openai-compatible`
connection also requires a `base_url`.

An Anthropic `base_url` is an explicit trusted-gateway override: the router
sends the configured Anthropic API key to that destination. Overrides must be
absolute HTTP(S) URLs without embedded credentials, query parameters, or a
fragment. Operators upgrading an existing deployment should verify every
non-null Anthropic `base_url`; leave it unset to use Anthropic's hosted API.

Provider connection tests are intentionally bounded: model discovery times out
after `CHALK_PROVIDER_PROBE_TIMEOUT_SECS` (default `5`) and at most
`CHALK_PROVIDER_PROBE_MAX_CONCURRENCY` probes run at once (default `8`).
Additional probes fail fast instead of occupying router tasks. Zero or invalid
values fall back to those defaults.

## Model resolution

The router resolves each request's `model` in this order, stripping any matched
prefix before forwarding:

1. **`chalk/<id>`** — looked up as a **published** model route (managed via
   `ModelRouteService` / the Model Routes UI). A simple route dispatches to its
   connection and `upstream_model_id`; a custom route resolves its default target
   and applies its configured judge and fallback policies. `chalk/` is reserved
   for routes and can never be a connection prefix.
2. **`<prefix>/model`** — the connection that owns `<prefix>/` (a built-in
   canonical prefix like `gemini/`, or a custom one like `chalk-gemma/`); the
   prefix is stripped and the rest forwarded unchanged.
3. **bare or unknown-prefix `model`** — the eligible OpenAI connection that owns
   `openai/` acts as the catch-all. If there is no canonical owner, a sole
   eligible OpenAI connection preserves the old behavior; multiple eligible
   non-canonical connections are rejected as ambiguous. For Anthropic-native
   endpoints, qualified prefixes and published aliases use the same resolution
   and stripping rules; bare IDs use the canonical-or-sole Anthropic connection.
   `explicit` connections never participate in implicit selection.

Examples: `"model": "gemini/gemini-2.5-flash"`,
`"model": "openrouter/anthropic/claude-sonnet-4"`, and
`"model": "chalk/my-alias"`. A connection's `exposure` controls `/v1/models`:
`dynamic` connections contribute their live catalog (with the prefix added to
every returned model ID), `unlisted` connections are callable by prefix but
omitted from the listing, and `explicit` connections are reachable only via a
model route.

## Judging and fallback

A custom route's `RouteGrid`
requires a default connection and upstream model, with optional judging and
fallback chains. There is no global or per-key judge-policy precedence. A judge
classifies the request by task or difficulty, and each outcome selects a concrete
connection/model and can supply its own fallback list. Backends are Local
heuristic, Completion, and Decisions; modes are Off, Shadow, and Live. Off and
Shadow serve through the default target; Live uses the selected outcome, or the
default target if classification is unavailable or unusable. Shadow records a
proposed decision without executing its generation targets.

Judging supports Chat Completions, Anthropic Messages, and self-contained OpenAI
Responses creation, including streaming. Responses using `previous_response_id`,
`conversation`, or a stored `prompt` skip judging because the classifier does not
fetch that context. Other request surfaces do not invoke the judge.

Chat Completions also accepts a request `fallbacks` map. For the selected model,
the precedence is: matching request entry, explicit judge-outcome list, route
fallback entry, then no alternatives. An empty list disables fallback; an omitted
entry inherits. Request keys and alternatives accept either an exact connection
name or ID followed by `/upstream_model_id`. Names are case-sensitive; if both
forms key the same model, the ID-keyed entry wins. Unmatched entries are ignored,
and request overrides never modify the saved route. For example:

```json
{
  "fallbacks": {
    "openai-prod/gpt-4o": ["openai-prod/gpt-5.6-luna"]
  }
}
```

The chosen chain is followed once, without expanding an alternative's chain or
judging again. Targets must be concrete connections/models, not other route
aliases. Every attempt is subject to permissions, capabilities, rate limits, and
quotas. Destinations the route owns (its default, judge outcomes, and its
fallback lists) are authorized and rate-limited under the requested `chalk/*`
model, so a key allowed that route may be served by them; each one's provider
and connection must still be within the key's scope. A request `fallbacks`
alternative is treated as if the caller had requested it directly: it is
authorized and rate-limited under its connection's prefixed model id and is
skipped when that connection is reachable only through a route. When no
destination survives these checks, the request fails with 400 if the request
cannot be represented, 403 if the key may not reach any destination, and 503
otherwise. The router stops starting new attempts five minutes into a request
but never cuts off an attempt in progress, except a background Responses
submission. Upstream connection failures/timeouts and HTTP 429/5xx can trigger
fallback, as can narrowly classified reasoning-support 400s from translated
Claude requests. Other client errors, unreadable responses, and answer-quality
problems are terminal. Once a stream
is committed, errors do not switch models. Surface-specific restrictions, such as
background Responses dispatch uncertainty, still apply.

For remote judging, an omitted or zero `timeout_ms` on a Completion or
Decisions judge backend uses the default of **1,000 ms**; zero does not disable
the deadline. The heuristic backend has no configurable timeout.
Explicit nonzero values must be 1–30,000 ms. If judging times out, selection uses
the route's default target and its applicable fallbacks. See the
[route decision grid](../rfp/0008-route-decision-grid.md) for behavior and
configuration details.

## Model capabilities

`GET /v1/models` and model detail responses include a schema-version-1
`capabilities` object resolved for that model, connection endpoint, and Chalk
adapter. It reports tri-state model surfaces and modalities plus concrete
interface operations and parameter constraints. Explicitly unsupported request
requirements are rejected before dispatch; unknown requirements remain
attemptable and upstream validation stays authoritative. Operators can refine
private or custom endpoints with the advanced `declared_capabilities` document
on a provider connection. The raw declaration remains connection-scoped, while
effective capabilities are exposed only through model list/detail. See
[RFP-0005](../rfp/0005-effective-per-model-capabilities.md) for the contract and
precedence rules.

## Seeding connections and routes

Connections and `chalk/*` routes can also be provisioned in bulk from a JSON
seed file with the `openai-server seed-connections [--file <path>]` subcommand
(or `ROUTER_CONNECTIONS_SEED_FILE`). Seeding is **insert-if-absent, not an
upsert**: an entry whose connection `name` (or route `public_model_id`) already
exists is skipped, never updated. Changing a field in the seed file for an
existing entry — a rotated `api_key`, a new endpoint or prefix — therefore has
no effect; the seeder logs a warning naming the diverged fields, but applying
the change requires the Providers UI or the update API.

## Local mock providers

`just mock-openai` and `just mock-anthropic` start the same synthetic upstream
with both API formats. For a native Anthropic connection on your local router,
follow the [local development prerequisites](development.md#local-database-and-secrets),
then seed the local connection once with
`just seed --file test_services/mock_openai/connections.local.json`,
then start the router with `just run`. The seed uses `http://localhost:18000/v1`
and the dummy key `unused`. Anthropic connections accept loopback base URLs
without an additional environment variable. See the
[mock setup guide](../test_services/mock_openai/README.md#native-anthropic-requests)
for the connection fields, sample routed requests, and cleanup behavior.

## Google: Gemini and Vertex AI

Google ships Gemini through two products that authenticate differently, so each
gets its own connection kind:

| Kind | Product | Credential |
| --- | --- | --- |
| `gemini` | Gemini Developer API (Google AI Studio) | API key |
| `vertex` | Vertex AI / Google Agent Platform (fka Vertex) | GCP service account key |

Gemini connections support image embeddings with `gemini-embedding-2` (and
`gemini-embedding-2-preview`) through `POST /v1/embeddings`. Supply a single
`image_url` part or combine text and images into one document:

```json
{
  "model": "gemini/gemini-embedding-2",
  "input": [
    {"type": "text", "text": "A dog on a beach"},
    {"type": "image_url", "image_url": {"url": "data:image/png;base64,..."}}
  ],
  "dimensions": 768
}
```

Image URLs accept HTTP(S) URLs or base64 image data URIs. The combined document
produces one vector; a list of strings produces one vector per string. Responses
use the OpenAI embedding format, including `encoding_format: "base64"` when
requested. Image requests require Google's reported token usage; the router
does not estimate image tokens. Text-only Gemini embedding models continue to
use Google's OpenAI-compatible API. See Google's
[embedding guide](https://ai.google.dev/gemini-api/docs/embeddings) for model limits.

The **Embeddings playground** accepts PNG/JPEG attachments with optional text.
The selected model's published capabilities determine whether text and images
are supported and which image counts and formats are allowed; aliases inherit
their resolved model's constraints. One run embeds all selected parts as one
document and returns one vector. Unknown image support is shown as unverified.

Attachments stay in browser memory with local previews until **Generate
Embedding** sends inline data through the playground proxy. The proxy uses the signed-in user's separate router API key; see [authentication](authentication-transition.md). Removal, Clear, or leaving
the tab releases previews; reloading discards
the draft. Provider errors and model changes retain attachments. Submitted
content follows the gateway's content-logging settings and provider retention.

Each playground request is limited to **4 MiB of UTF-8 encoded JSON**, including
text and base64 data. This permits roughly 3 MiB of image bytes before text and
JSON overhead. This playground limit is separate from model/provider limits
and the direct gateway body limit. It was selected from local production
transport measurements against the UI's 256 MiB deployment budget: four
overlapping 4 MiB requests peaked around 205 MiB RSS, while four 8 MiB requests
reached 310 MiB. The proxy bounds actual bytes before forwarding, including
requests without `Content-Length`, and leaves JSON validation to the gateway.
Quotas, token usage and billing continue to run in the gateway; the browser
does not estimate image tokens or cost.

Vertex accepts no API key. A `vertex` connection stores the **service account
key JSON** — the blob GCP downloads, holding a PEM private key — as its
credential, and the router signs an RFC 7523 JWT-bearer assertion with it,
exchanges that for a `cloud-platform` access token, caches the token, and
refreshes it two minutes before expiry. A burst of concurrent requests on a cold
cache mints one token between them. The private key is used locally to sign
assertions; the access token is sent upstream. Automatic access-token refresh
does not replace the operator's service-account key rotation policy. An `authorized_user` blob from
`gcloud auth application-default login` also works, for local development.

The project comes from the key itself. The region defaults to `us-central1` and
can be changed either by adding a `"location"` field to the stored key JSON or
by pointing the connection's `base_url` at another
`https://<region>-aiplatform.googleapis.com` (`https://aiplatform.googleapis.com`
is the `global` endpoint).

Chat completions and streaming use Vertex's OpenAI-compatible endpoint, so their
request and SSE shapes match every other provider. A bare model name is
qualified with the `google` publisher (`vertex/gemini-2.5-pro` →
`google/gemini-2.5-pro` upstream); partner models keep their own
(`vertex/meta/llama-3.1-405b-instruct-maas`). Most text embeddings use the native
`:predict` call. Gemini Embedding 2 instead uses `:embedContent`, supporting
text, images, and mixed documents with requested dimensions. `/v1/models`
lists the region's publisher catalog. Image, audio, and moderation requests
return an explicit error rather than silently routing elsewhere.

```bash
curl -X POST http://localhost:4001/v1/chat/completions \
  -H "Authorization: Bearer $ROUTER_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model": "vertex/gemini-2.5-pro", "messages": [{"role": "user", "content": "hi"}]}'
```

## Image generation parameters

`/v1/images/generations` validates advertised per-model parameter constraints
before carrying provider-specific fields through to the upstream provider. The
Playground renders controls from the selected model's advertised image
parameters:

- **OpenAI gpt-image** — `size`, `quality`, `n`, `background`
  (`auto`/`transparent`/`opaque`), `output_format` (`png`/`jpeg`/`webp`).
- **Gemini playground** — uses native Interactions when advertised, sending
  `aspect_ratio` and `image_size` under `response_format`. OpenAI-only size,
  quality, and count controls are hidden. Requests set an 8,192-token output
  limit for quota estimation.
- **Gemini OpenAI-format clients** — `/v1/images/generations` still sends
  `aspect_ratio` and `image_size` as `extra_body` passthrough fields to Google's
  compatibility endpoint; this does not support every Gemini image model.
