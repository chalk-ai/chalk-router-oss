---
title: Health and trace ingestion
---

## Health

HTTP health handlers are unauthenticated and simple:

| Path | Response |
| --- | --- |
| `GET /health` | 200, plain text `OK` |
| `GET /v1/healthcheck` | 200, JSON `{"profile":"chalk-router","status":"ok"}` |

These HTTP responses do not perform provider inference or report per-provider readiness. gRPC health has separate startup/readiness state:

```sh
grpcurl -plaintext \
  -import-path proto -proto grpc/health/v1/health.proto \
  -d '{"service":"readiness"}' \
  127.0.0.1:4001 grpc.health.v1.Health/Check
```

Only `startup` and `readiness` service names are supported; empty/unknown names return NOT_FOUND. Watch is declared server-streaming but always returns UNIMPLEMENTED. [Generated health reference](/reference/grpc-health/).

## Client traces

`POST /v1/traces/spans` is a router extension for client session, turn, reasoning, and tool spans. It takes [IngestSpansRequest](/reference/trace-types/#ingestspansrequest) and returns [IngestSpansResponse](/reference/trace-types/#ingestspansresponse), whose `accepted` field counts accepted spans. Normal inference authentication applies.

Each span supplies name, kind, trace ID, span ID, start/end nanosecond timestamps, and optional parent/status/attributes. Trace IDs are 32-character nonzero lowercase hex; span IDs are 16-character nonzero hex. [IngestSpan](/reference/trace-types/#ingestspan), [IngestSpanKind](/reference/trace-types/#ingestspankind), and [UnixNanos](/reference/trace-types/#unixnanos) define fields and encodings.

Use decimal strings for nanosecond timestamps from JavaScript to avoid integer precision loss. Generate current timestamps when sending: the handler allows at most 1,024 spans, seven-day age, 24-hour duration, and five-minute future clock allowance. Arbitrary historical examples will fail these checks. String/bool/number attributes are recorded natively; nested arrays/objects become JSON strings.

Telemetry export depends on configuration and sampling. An accepted ingestion response is not a promise that an external collector has persisted a span.

Sources: [HTTP health handlers](https://github.com/chalk-ai/chalk-router/blob/main/src/main.rs), [gRPC health](https://github.com/chalk-ai/chalk-router/blob/main/src/grpc/standard_health.rs), [trace handler and tests](https://github.com/chalk-ai/chalk-router/blob/main/src/handlers/traces.rs).
