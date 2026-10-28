# Mock OpenAI and Anthropic test service

[Documentation index](../../docs/README.md) · [Local setup](../../docs/development.md#local-database-and-secrets)

This is a deployable synthetic OpenAI- and Anthropic-compatible upstream for
router development and benchmarks. It is not an integration test and is not
part of the router image. The router's in-process integration-test mocks
remain the right tool for fast, focused assertions; this service is for
exercising a running router through a real upstream HTTP boundary.

The service is intentionally usable in two places:

- **Local development:** a developer starts it on localhost and registers a
  dedicated provider connection in their local router.
- **Benchmarks:** Buildkite deploys the commit-pinned image as a private GKE
  service in the same cluster and region as the router being measured.

Every scenario knob (latency, jitter, response length, error rate, 429 rate,
per-scenario concurrency cap) is encoded directly in the `model` string, so a
normal OpenAI or Anthropic client drives it without protocol changes:

```
mock/l<base_ms>-j<jitter_ms>-t<tokens>-e<err_pct>-r<429_pct>-c<max_concurrency>
```

Any field may be a range `<lo>-<hi>`, resampled per request. For example,
`mock/l50-500-t100-2000` gives every request a latency in `[50, 500]ms` and a
response length in `[100, 2000]` words. All fields are optional; defaults are
`l0 j0 t50 e0 r0 c0` (`c0` means no cap).

Presets: `mock/fast`, `mock/slow`, `mock/huge`, `mock/flaky`, and
`mock/throttled`.

Concurrency slots remain held until an entire response finishes, including
streaming delivery. Disconnects and canceled requests release their slots.
`/debug/stats` counts queued requests and active responses in `in_flight`;
it returns to zero after both drain.

Streaming responses include a final usage chunk with empty `choices` before
`[DONE]` by default. This supports router usage accounting even when a generic
`openai-compatible` connection strips `stream_options`. Counts match
nonstreaming responses and use synthetic word counts, not a model tokenizer.
Direct clients can set `stream_options.include_usage: false` to test missing
usage failures.

Endpoints: `POST /v1/chat/completions` and `POST /v1/messages` (both sync and
SSE streaming), `POST /v1/messages/count_tokens`, `POST /v1/embeddings`,
`GET /v1/models`, `GET /health`, and `GET /debug/stats`.

## Local development

Start the service in the foreground (Ctrl-C stops it). The command creates a
local Python 3.13 virtual environment and installs its Python dependencies on
the first run. Dependency versions are locked in `uv.lock`:

```bash
just mock-openai
just mock-openai 18010
```

The command prints the provider base URL. With the default port it is:

```
http://127.0.0.1:18000/v1
```

Register a dedicated connection through the Providers UI or admin API. For a
local seed used only to create the connection the first time, use:

```json
{
  "connections": [
    {
      "name": "mock-openai-dev",
      "kind": "openai-compatible",
      "base_url": "http://127.0.0.1:18000/v1",
      "api_key": "unused",
      "exposure": "unlisted",
      "prefix": "mock/"
    }
  ]
}
```

Use the admin API or UI to change an existing connection. The router's seed
command is insert-only, so it deliberately cannot update an existing endpoint.
Do not repoint the built-in `openai` connection in a shared environment: the
dedicated `mock/` prefix makes synthetic traffic visible and keeps real traffic
isolated.

### Native Anthropic requests

The service also implements the native Anthropic Messages API. It accepts the
same `mock/<profile>` model IDs and scenario knobs, returns Anthropic-shaped
responses and errors, and streams the normal message/content-block event
sequence. Token counts are deterministic word-count estimates; they are useful
for exercising router control flow, not for comparing against Anthropic's
tokenizer.

```bash
curl -N -X POST http://127.0.0.1:18000/v1/messages \
  -H 'content-type: application/json' \
  -H 'anthropic-version: 2023-06-01' \
  -d '{"model":"mock/fast","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"hello"}]}'
```

To use it through your local router, run these commands from the repository
root with your local, disposable Postgres database available and migrations
applied through `just migrate`. The `just seed` / `just run` recipes also require
SOPS access even though the mock uses dummy provider credentials. This local
loopback setup is for environment mode; non-legacy cluster environments reject
loopback destinations under the [cluster egress policy](../../docs/cluster-router.md).

1. Seed the local connection once:

   ```bash
   just seed --file test_services/mock_openai/connections.local.json
   ```

   The [local seed file](connections.local.json) creates a persistent
   `mock-anthropic-dev` connection. Seeding skips an existing connection with the
   same name; use the Providers UI to update its fields if needed. This file is
   for local development and is separate from the Kubernetes seed template.

2. Start `just mock-anthropic` (an alias for `just mock-openai`; run only one).
3. In another terminal, start the router:

   ```bash
   just run
   ```

   `just run` enables anonymous local proxy requests through
   `ROUTER_DANGEROUSLY_PERMIT_UNAUTHENTICATED`; no client key or
   Anthropic-specific environment variable is required. For trusted local access,
   start `just ui` to test the seeded connection in the Providers UI; see the
   [current UI boundary and pending changes](../../docs/authentication-transition.md).
   You can also create it manually
   with these fields:

   | Field | Value |
   | --- | --- |
   | Name | `mock-anthropic-dev` |
   | Provider kind | `anthropic` |
   | Base URL | `http://localhost:18000/v1` |
   | API key | `unused` (dummy value; the mock does not authenticate) |
   | Prefix | `mock-anthropic/` |
   | Exposure | `unlisted` |

4. Send a request through the router:

   ```bash
   curl --fail-with-body -N http://127.0.0.1:4001/v1/messages \
     -H 'content-type: application/json' \
     -H 'anthropic-version: 2023-06-01' \
     -d '{"model":"mock-anthropic/mock/fast","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"hello"}]}'
   ```

   `mock-anthropic/` selects the connection; the upstream receives `mock/fast`.
   Set `stream` to `false` for a regular response. The same model also works on
   `/v1/chat/completions` to exercise OpenAI-to-Anthropic translation, and on
   `/v1/messages/count_tokens` with a Messages token-count request.

Anthropic connections accept an HTTP(S) base URL on **`localhost` or a numeric
loopback IP** (`127.0.0.1` or `[::1]`) without a separate process setting.
Other targets retain the public-host checks; non-loopback private addresses
and `.localhost` subdomains remain rejected. Probes require other hostnames
to resolve only to public addresses. Local upstream requests bypass HTTP
proxies and do not follow redirects. The client pins `localhost` to IPv4/IPv6
loopback addresses instead of relying on system DNS or hosts-file entries.
For another port, run `just mock-anthropic <port>` and update the connection's
base URL to match. The mock serves its APIs under `/v1`. The router and mock
must run on the same host/network namespace.

Keep real Anthropic connections separate and use a dummy key for this mock.
Disable routing on or delete the connection when you want to stop using it.
Loopback support does not enable a connection to a separate private-cluster
Anthropic mock service.

Run the mock's contract tests with `just mock-openai-test`; `just ci` covers
the Rust endpoint-validation and routing integration tests.

## Regression tests

Run `just mock-openai-test` from the repository root. The tests exercise the
ASGI response lifecycle directly, including backpressure, disconnects, and
cancellation, without making provider requests. They also run on pull requests
that change the mock.

## Benchmark deployment

Build this directory's Dockerfile into your own registry and deploy `deploy/chart`
into an isolated benchmark namespace. Configure its image repository/tag explicitly.
Use the service's cluster-local URL (for example,
`http://mock-openai.benchmark.svc.cluster.local:8000/v1`).

Before a benchmark, its setup step must create or update a dedicated
`bench-mock-openai` connection to that URL through the router admin API, then
record the router revision, mock-service image revision, connection ID, and
scenario in the result. This is intentionally outside the Helm chart: a router
connection belongs to the environment under test, while this chart owns only
the upstream service.

The chart has no public ingress. Keeping the synthetic upstream in the same
cluster and region avoids public-ingress, cross-platform, and Internet latency
from becoming part of a router benchmark. The old Chalk Compute deployer was
removed for that reason; use a separate, explicitly named external-network
benchmark if exercising that path is the goal.

The benchmark chart deliberately runs one replica and the image runs one
Uvicorn worker. Therefore the `c` cap and `/debug/stats` counters describe the
whole deployed service. If either value is increased, those counters and caps
become per replica/process rather than global and must not be treated as an
aggregate benchmark signal.

## Example

```bash
curl -X POST http://127.0.0.1:18000/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model": "mock/l1000-j2000-t200-e5-r10-c50", "messages": [{"role": "user", "content": "hi"}]}'
```
