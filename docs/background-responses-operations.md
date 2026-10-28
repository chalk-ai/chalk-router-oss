# Background Responses operations

[Documentation index](README.md) · [Background API behavior](api.md#background-responses)

## Setup

Background processing is always enabled. Restart-safe, multi-replica accounting
and enforcement require `ROUTER_DATABASE_URL`, `ROUTER_USAGE_BACKEND=postgres`,
and `ROUTER_QUOTA_BACKEND=postgres`. Without Postgres, job metadata and memory
quotas are volatile and per-process. Check configuration and startup logs when
investigating accounting failures; usage reporting initializes independently of
job storage.

Apply the current migrations before serving. Settlement timestamps,
retention/ownership indexes, and restrictive foreign keys support cleanup.

## Inspect jobs

Use the existing admin credential (`ROUTER_ADMIN_KEY` or an authorized Chalk
client JWT), not an inference API key. Listing is read-only and scoped to the
selected environment: `CHALK_ENVIRONMENT_ID` in environment mode, or the
authenticated runtime in [cluster mode](cluster-router.md). For JWT grant
changes proposed in PR #353, see the [authentication transition](authentication-transition.md).

From the repository root, target a direct router gRPC endpoint if your gateway
does not expose admin gRPC paths. Omit `-plaintext` for TLS:

```sh
grpcurl -plaintext -import-path proto \
  -proto chalk/router/v1/background_response.proto \
  -H "Authorization: Bearer $ROUTER_ADMIN_TOKEN" \
  -d '{"limit":40}' "$ROUTER_GRPC_ADDR" \
  chalk.router.v1.BackgroundResponseService/ListBackgroundResponses
```

The default list excludes settled jobs. Filter by `phase`: `submitting`,
`active`, `terminal`, `unresolved`, or `settled`. Pass `nextCursor` as `cursor`
with the same filter for another page. Pages default to 40 and are capped at 100.

Metadata includes event/response IDs, connection, requested/served models,
phase/status, timestamps, poll attempts, next-poll time, claim expiry, and a
safe error classification. Prompts, outputs, credentials, creator key hashes,
trace baggage, raw errors, and internal payloads are never returned.

## Automatic polling and accounting

The worker checks existing upstream responses through their original connection;
it **does not retry generation**. Terminal responses (completed, failed,
cancelled, or incomplete) settle accounting and quotas. Temporary polling or
storage failures back off and retry automatically. Persisted terminal evidence
allows settlement without another upstream retrieval.

- `active` with `queued`/`in_progress` can be healthy; age alone is not failure.
- `terminal` means accounting is pending. Inspect storage availability/logs.
- `unresolved` without a response ID may represent an accepted submission whose
  acknowledgement was lost. Do not assume zero usage or regenerate it.
- A future `claimUntil` means a worker is processing the job. Do not clear the
  claim or force-release its quotas.
- `upstream_or_settlement_unavailable` is intentionally generic. Use existing
  structured logs to investigate, repair storage or the original connection,
  then allow automatic reconciliation to continue.

There is no manual reconciliation or quota-release RPC. Existing settlement
telemetry remains unchanged; no new metrics subsystem is added.

## Retention

`ROUTER_BACKGROUND_RESPONSES_RETENTION_DAYS` defaults to **30**. Setting it to
`0` disables cleanup only, not polling, tracking, budgets, or rate limits.
The existing worker checks approximately every five minutes when available,
deleting at most 100 jobs per pass. PostgreSQL uses `FOR UPDATE SKIP LOCKED` and
2-second statement, 500-ms lock, and 5-second overall timeouts. Busy or failed
passes retry later. Memory cleanup uses a bounded settlement expiry index.

Only `settled` metadata older than the retention period **since settlement**,
without an active claim or owned quota reservation/lease, is eligible. Cleanup
preserves usage events/costs, quota counters, non-settled jobs, and upstream
responses. Retrieval after pruning does not create duplicate usage events.
Disable retention through a normal rollout if needed; pruned metadata cannot
be recovered from the router.

## Verification

Run `just ci` and `just test-db-docker --no-fail-fast` against disposable
Postgres. Tests cover both stores, pagination, privacy/authentication,
settlement-age and batch bounds, environment isolation, row-lock skipping,
ownership protection, and unchanged accounting after pruning.

For an optional paid staging smoke test, submit a bounded background request
through a retrieval-capable connection, leave it unpolled, and confirm it
becomes `settled` with one usage event. Retrieve it twice and confirm accounting
does not change. Do not backdate production jobs to test retention; aging is
covered only in disposable test databases.
