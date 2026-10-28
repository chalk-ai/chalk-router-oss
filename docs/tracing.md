# Tracing and telemetry

[Documentation index](README.md) · [Repository overview](../README.md)

## Span contract and content logging

All LLM spans conform to the OpenInference semantic conventions — see
[`AGENTS.md`](../AGENTS.md) for the span contract (helpers in
`src/handlers/trace_attributes.rs`) and the exporter setup in
`src/telemetry.rs`. `ROUTER_TRACE_LOG_CONTENT` defaults to `false`, recording span metadata only
(model, token counts, latency) without prompt/response content.

Completed, priced LLM calls also carry `llm.cost.{total,prompt,completion}` as
`f64` USD estimates. `chalk.cost.source=estimated` and
`chalk.cost.rate_card_version=<effective timestamp>` identify the checked-in
LiteLLM card used. These values are public list-price approximations for traces
and evaluations: they exclude negotiated rates, discounts, batch pricing,
long-context tiers, regional differences, and other pricing dimensions absent
from the catalog. An unpriced model or an unknown provider-side cache rate omits
cost rather than reporting a false zero. A router response-cache hit is the
exception: it reports zero upstream cost with
`chalk.cost.source=router_cache`, because no provider request was made.

When content capture is explicitly enabled, `input.value` contains the complete typed caller request,
including messages, tools and accepted options. Unknown fields discarded during
deserialization are not captured. Historical spans contain only the messages
array. Flattened input message attributes
retain their original indices and prioritize leading system messages plus the
newest remaining messages, with dedicated capacity reserved for advertised
tools. Output metadata receives
the larger share of the remaining span budget and is written before optional
content. The budgets are derived from `OTEL_SPAN_ATTRIBUTE_COUNT_LIMIT` (128 by
default), the same value configured on the SDK. Direction-specific truncation is
reported in `chalk.trace.{input,output}.truncation` only when it occurs.

## Large payloads and exporter limits

Chat, Anthropic Messages, and embeddings store raw input or output JSON larger
than 64 KiB under `ROUTER_OBJECT_STORE_URI`; the span records its byte count and
URI. Upload work is concurrency-limited, retried once, and bounded by a five-second
timeout. It is also memory-bounded from the container's cgroup limit: retained
upload bodies are capped at the smaller of 1/32 of the pod limit and 128 MiB.
The router never substitutes node
memory for a missing pod limit. Without object storage—or when runtime, queue, or
memory admission rejects an upload—the raw value is omitted, structured metadata
is retained, `chalk.trace.{input,output}.upload_omitted` explains why, and the drop
is logged. When no finite cgroup limit exists, upload bodies use a fixed 128 MiB
budget rather than node memory. Upload retries share one resident body buffer;
uploads are not streaming because the complete JSON is serialized before
admission. Standard `input.mime_type` / `output.mime_type` attributes are present
only when the corresponding inline value is present.

The client span ingest API retains its documented 1,024-span request limit,
independent of exporter queue capacity. Exporter queues use the standard
`OTEL_BSP_MAX_QUEUE_SIZE` setting (2,048 by default), while export batches
contain at most 16 spans. Client-ingested names, status messages, and attributes
are bounded to the same per-span envelope, keeping batches below the collector's
4 MiB gRPC message limit. Lower `OTEL_BSP_MAX_EXPORT_BATCH_SIZE` values are
still respected.

## Client span ingestion

Client-ingested spans are the client's report, so the router marks them and
attaches what it verified:

- Every ingested span carries `chalk.span.source=client`, so token counts a
  client reports can be told apart from the router's own metered LLM spans.
- The verified caller is stamped as `chalk.auth.principal.id` and
  `chalk.auth.principal.type`, plus `user.id` for federated users (router API
  keys are never users). Any `chalk.*` or `user.id` attributes the client sends
  are dropped first, so these values always come from authentication, and they
  survive when content logging is off.
- Timestamps must be plausible: a span may end at most 5 minutes ahead of the
  router's clock, start at most 7 days ago, and last at most 24 hours. Anything
  else rejects the request with 400.

The router reserves seven attribute slots per ingested span for these and for
the session, environment and sampling attributes, so a client span may carry at
most the SDK attribute limit minus seven.

Every span the router starts goes to the OTLP collector at
`OTEL_EXPORTER_OTLP_ENDPOINT` (gRPC).

## Braintrust

Setting `BRAINTRUST_API_KEY` adds a second, independent destination for the
**frontend** spans — the per-request span of an inbound LLM route
(`/v1/chat/completions`, `/v1/messages`, `/v1/embeddings`, `/v1/images/*`,
`/v1/audio/*`, `/v1/moderations`, `/v1beta/interactions`) and every span carrying an
`openinference.span.kind`, which covers the handlers' LLM/embedding spans and
the agent spans replayed through `/v1/traces/spans`. Admin RPCs, health probes,
and the router's internal spans are not forwarded; they keep going to the OTLP
collector like everything else, which is unaffected either way.

Braintrust reads the OpenInference conventions natively, so a chat completion
arrives as an `llm` span with parsed `input`/`output` messages and token
metrics, nested under a `POST /v1/chat/completions` root labelled with the API
dialect it arrived on (`chalk.frontend` = `openai` / `anthropic` / `gemini`). Nothing is
sent when no key is configured. For content between 64 KiB and 256 KiB that the
router moved to object storage, the Braintrust exporter resolves the URI into a
temporary exporter-only `input.value` or `output.value`; the shared OTLP span
remains bounded. Hydration is best-effort and has a three-second sub-deadline
inside the ten-second export window, so a degraded object store cannot consume
the whole Braintrust POST budget.

| Variable | Default | Meaning |
| --- | --- | --- |
| `BRAINTRUST_API_KEY` | — | Enables export. Unset ⇒ no exporter is installed. |
| `BRAINTRUST_PROJECT` | `chalk-router` | Project name spans land in. |
| `BRAINTRUST_PROJECT_ID` | — | Target a project by id instead of by name. |
| `BRAINTRUST_PARENT` | — | Raw `x-bt-parent` value (e.g. `experiment_id:…`); overrides both of the above. |
| `BRAINTRUST_API_URL` | `https://api.braintrust.dev` | API root, or a full `…/v1/traces` collector URL. |

`ROUTER_TRACE_LOG_CONTENT=false` gates prompt/response content here too — the
attributes it suppresses are never set, so they cannot reach Braintrust.

### Live verification

The optional paid live round-trip test (`test_braintrust_frontend_span_round_trip` in
`tests/integration_test.rs`) exports through the real ingest endpoint and reads
the spans back out of the Braintrust API:

```bash
ROUTER_LIVE_API_TESTS=1 BRAINTRUST_API_KEY=… \
  just test-live braintrust
```
