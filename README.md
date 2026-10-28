# chalk-router

Chalk Router is a Rust/Axum LLM gateway with OpenAI-compatible and
Anthropic-compatible HTTP APIs, native Gemini Interactions, and multiple
upstream providers. It exposes a gRPC/gRPC-Web admin API and a Next.js UI for
API keys, providers, model routes, usage, quotas, and telemetry. The Rust crate
and binary are named `openai-server`.

[API documentation site](docs-site/README.md): source-generated HTTP and gRPC references,
integration guides, and local preview with `just docs-dev`.


Start with [building and running](#building--running), or use the
[documentation index](docs/README.md) to find a guide. Contributors should read
[AGENTS.md](AGENTS.md) for repository conventions and operational safeguards.

## Endpoints

<a id="proxied-llm-api-implemented"></a>
<a id="native-gemini-interactions"></a>
<a id="admin--management-surface"></a>
<a id="stubbed--partial"></a>

See the [API reference](docs/api.md) for implemented endpoints, protocol-specific
behavior, background Responses, admin pagination, and unsupported operations.

## Model routing

<a id="local-mock-providers"></a>
<a id="google-gemini-and-vertex-ai"></a>
<a id="image-generation-parameters"></a>

[Model routing and providers](docs/model-routing.md) covers connections,
`chalk/*` aliases, model permissions, judging, fallback, and Google integrations.
Provider credentials are stored in connections, not server environment variables.

## Authentication

Federation is opt-in. Admin JWTs require a pinned audience, the operation's
permission, and verified environment/team ownership. Static admin credentials
remain available to trusted operators; inference applications should use scoped
router API keys. Standard HTTP/gRPC health probes remain public.

The UI requires a per-user session and forwards that user's JWT for admin RPCs.
Playground requests use a separate user-supplied router API key. There is no shared
server admin-key fallback. See [authentication and sessions](docs/authentication-transition.md)
for configuration, permission checks, and session limits.

## Prompt caching (exact prefix)

<a id="per-token-policy"></a>
<a id="backends"></a>

See [prompt caching](docs/prompt-caching.md) for opt-in API-key policy,
environment isolation, backend settings, and failure behavior.

## Client integrations

The [Chalk CLI guide](docs/router-cli.md) covers Codex, Claude Code, OpenCode,
VS Code, and Cursor setup and verification.

## Tracing

<a id="braintrust"></a>

See [tracing and telemetry](docs/tracing.md) for OpenInference spans, content
logging, storage limits, client span ingestion, OTLP, and Braintrust.

## Building & running

### Prerequisites

- Rust + Cargo (the crate uses edition 2021), and `just`
- Git access to the `chalk-federated-auth` dependency
- For the UI: Node.js 22 and npm (`npm --prefix ui ci`)
- A Postgres database if you want persistence (see [migrations](#database--migrations));
  without one the router uses in-memory stores for local dev

### Build

```bash
just build
```

### Run

```bash
export ROUTER_ADMIN_KEY="$(openssl rand -hex 32)"
just run
```

The router binds `127.0.0.1:4001` and requires authentication. `just run` does not
load company secrets or select a database. A local `.env` is still read by the
binary; explicit environment variables take precedence. Without a database,
state is in memory and disappears on restart. See [self-managed setup](docs/self-managed-setup.md)
for persistence, explicit seeding, and key formats.

From another terminal using the same key:

```sh
curl -H "Authorization: Bearer $ROUTER_ADMIN_KEY" http://127.0.0.1:4001/v1/models
```

Configure `ROUTER_UI_ORIGIN` and `ROUTER_UI_SESSION_SECRET` before `just ui`;
see [authentication](docs/authentication-transition.md). The UI binds loopback.
To bind the backend elsewhere, explicitly set `ROUTER_BIND_ADDRESS`. Anonymous
inference is an explicit development option; a non-loopback anonymous listener
requires a second dangerous opt-in, and cluster mode rejects anonymous access.

### Example: chat completion

Create a provider connection first; no provider keys are read from server
environment variables. Set `ROUTER_API_KEY` to a router credential authorized for
the selected model.

```bash
curl -X POST http://localhost:4001/v1/chat/completions \
  -H "Authorization: Bearer $ROUTER_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "anthropic/claude-sonnet-4-5-20250929",
    "messages": [{"role": "user", "content": "Hello, how are you?"}]
  }'
```

## Architecture

The [development guide](docs/development.md#architecture) maps the HTTP inference
handlers, gRPC admin adapters, shared services, stores, and UI. The
[RFP index](rfp/README.md) preserves design decisions and their historical context.

## Database & migrations

The application does not create tables at boot. Follow the
[migration guide](migrations/README.md) for local guards, schema snapshots,
deployment coordination, and recovery. Migrate only a disposable local database
during development; shared database changes belong to the approved deployment
workflow. [Cluster mode](docs/cluster-router.md) serves multiple environments
without per-environment schema migrations.

## Testing

Use the [development checks](docs/development.md#testing): `just ci` is the Rust
gate, and database tests require a disposable Postgres instance. UI, synthetic
mock, benchmark, and optional paid live tests have separate instructions there.

## Deployment

<a id="workload"></a>
<a id="networking"></a>
<a id="secrets"></a>
<a id="non-secret-env"></a>
<a id="migrations-on-deploy"></a>
<a id="object-storage"></a>

The [deployment guide](docs/deployment.md) describes generic self-managed
configuration, hardened workloads, optional ingress, and migration ordering.
The Files upload API is disabled for every caller; see [file security](docs/file-api-security.md).
Dated changes remain in [changelogs](changelogs/README.md).

### Deleting model routes

`DELETE /v1/models/{model_id}` is unsupported and returns 501 after normal inference authentication; it never deletes aliases or provider models, including for static admin credentials and anonymous development mode. Use `ModelRouteService/DeleteModelRoute` through the authorized gRPC admin API to delete router aliases.
