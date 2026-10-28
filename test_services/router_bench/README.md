# Chat Completions load benchmark

[Documentation index](../../docs/README.md) · [Development checks](../../docs/development.md#testing)

Measures `/v1/chat/completions` through real localhost HTTP connections to the
existing [mock OpenAI service](../mock_openai/README.md). Requires Docker,
`uv`, and the release router. It never accepts an existing database or router URL.
Results are not checked in; outputs stay local and ignored.

```bash
just bench-chat-build
just bench-chat --output /tmp/chat-bench-results
```

The default Postgres 16 image uses the repository's digest pin. To use a cached
local image, pass `--postgres-image postgres:16.14-alpine`; the exact image ID
and server version are recorded. No encrypted environment files or real
provider credentials are loaded. Child processes start outside the repository
to prevent the router's dotenv loader from finding a development `.env`.

## Matrix

Each cell uses an authenticated, connection-restricted API key and durable
Postgres usage writes. Prompt cache and judge are off. The upstream emits eight
words with no synthetic latency, jitter, errors or throttling. An explicit
64-token output cap makes budget reservation possible.

| Dimension | Values |
| --- | --- |
| Quota | `disabled`, `none`, `rate`, `budget`, `both` |
| Sampling | `disabled`, `all_content`, `all_metadata`, `ten_percent`, `drop`, `outcome` |
| Delivery | JSON and SSE |
| Concurrency | 1 and 16 |
| Repetitions | 3, with randomized cell order |

`disabled` turns off enforcement while preserving auth and durable usage.
`none` leaves Postgres enforcement on with no matching definitions. `rate`
adds global and connection-scoped requests-per-minute limits; `budget` adds a
daily token budget; `both` combines them. Limits are intentionally high enough
that this measures admission and reconciliation, rather than expected denials.
One shared key is used by default to expose contention. `--keys 16` assigns
separate keys to workers, isolating per-key contention.

Sampling `disabled` and `all_content` retain all traces and record content;
`all_metadata` retains all traces without content. `ten_percent` keeps 10% with
content logging on; `drop` keeps 0% with content logging off. `outcome` starts at
10%, logs no content, and overrides to 100% when `choices.0.finish_reason`
equals `stop`. Optional `drop_content` isolates export gating from content
logging; `rules_1000` measures 1,000 nonmatching request rules.
`outcome_all` starts at 100%, logs no content, and has the same keep-all outcome
rule. Comparing it with `all_metadata` controls exported work while isolating
the cost of checking an outcome. `outcome` additionally exercises promotion.

To stress every rate dimension (RPM, TPM and concurrency, at both global and
connection scopes), run:

```bash
just bench-chat --output /tmp/chat-bench-all-limits \
  --rate-dimensions all --quotas rate both --sampling all_metadata
```

To stress interleaved policy lock ordering,
use the owned fixture's `--quota-order interleaved`. This remaps only internal
policy IDs in the fresh database before runtime states exist, putting
concurrency policies before token policies. The default uses random IDs.

```bash
just bench-chat --output /tmp/chat-bench-lock-order \
  --postgres-image postgres:16.14-alpine \
  --duration 2 --warmup 0.3 --repeats 1 --concurrency 16 \
  --sampling all_metadata --quotas both --rate-dimensions all \
  --quota-order interleaved
```

Repeat with `--keys 16 --repeats 3` and a new output directory to isolate
shared-key contention. Run matching-source load comparisons sequentially so
stacks do not compete for CPU or database resources.

## Multi-tenant quota comparison

`quota_tenants.py` compares router binaries on the cases quota changes must
balance: no definitions at all (`simple`), one hot key (`hot`), many keys
sharing usage-pool rows (`tenants`), and many keys with no shared rows
(`indep`). Every key and pool carries key/pool and connection-scoped RPM, TPM
and concurrency limits plus a daily token budget, so both admission phases and
terminal settlement do real work. Each pass repeats at every `--db-delay-ms`,
added as a netem delay inside the Postgres container to model a remote
database; a local database hides the cost of every extra round trip.

```bash
just bench-quota-tenants --output /tmp/quota-tenants \
  --postgres-image postgres:16.14-alpine \
  --binary before=/path/to/base/openai-server --binary after=target/release/openai-server
```

Versions alternate order across `--repeats` (default 3), each on a fresh stack.
After every cell the database must hold one usage row per success and no
residual receipts, leases or active counts; any non-200 response or quota
backend error aborts the run. `summary.md` holds the median table. Nonzero
delays need passwordless `sudo` for `nsenter` and `tc`; pass `--db-delay-ms 0`
otherwise.

Cells with backend errors are recorded as `healthy=false`; the command completes
the matrix and exits unsuccessfully. These cells are diagnostics, not healthy
throughput measurements. This catches fail-open quota errors even when all
HTTP responses are 200. Response/usage mismatches abort the run immediately.

## Measurement and isolation

The aiohttp driver uses a closed loop with persistent HTTP/1.1 connections, one worker
per concurrent request, and no retries. It records successful requests/second,
full-response p50/p95/p99, and SSE time to the first content token separately
from response headers. SSE success requires content, `stop`, valid usage and
`[DONE]`, and it reads to EOF. Direct-to-mock controls run before and after each
repetition. Warmup primes routing/policy caches; the measured phase opens fresh
client connections, amortized over the requests in that phase.

Sampling policy updates reach the serving snapshot on a five-second refresh
cycle. The harness groups cells by policy and restarts its owned router after
each policy change, so startup primes the persisted policy before warmup.
Export/content checks reject mixed-policy or unexpectedly missing exports.
Cell order is randomized within these policy/backend groups, and group order
is randomized independently for each repetition.

Each measured cell captures process CPU, `pg_stat_statements` calls and execution
time (including nested function statements), Postgres deadlocks, quota backend
errors, exact durable usage row counts, and accepted OTLP exports. The local
collector returns successful export acknowledgements; a nonexistent collector
would measure repeated export failures instead. Export counters include a
short batch-drain window and are approximate at policy boundaries. SQL rows
marked `toplevel=false` are nested work, so summing them with their parent would
double-count execution time. Aggregate process CPU is not a sampled CPU
profile. Host load and CPU/I/O/memory pressure snapshots make interference
visible. WAL counters and write/sync milliseconds come from `pg_stat_wal`;
they are asynchronously updated whole-database totals, not per-request spans.

For CPU call stacks and Postgres wait sampling on the owned processes:

```bash
just bench-chat-profile --output /tmp/chat-quota-profile \
  --quotas both --rate-dimensions all --sampling all_metadata \
  --delivery json --concurrency 1 16 --duration 25 --control-duration 1 --warmup 1 --repeats 1
```

This requires `sudo -n perf`, records CPU-clock stacks at 199 Hz, and observes
active Postgres backends every 20 ms. CPU percentages exclude time asleep;
wait observations are across backends, not percentages of request latency.
Reports include self CPU symbols and call stacks for both processes. Timings
from these instrumented runs are diagnostic; use `bench-chat` for primary
latency measurements. Raw profiles stay in the local output directory.

Optional `--sql-profile --diagnostic-patch test_services/router_bench/pool_timing.patch`
collects SQLx client query durations and pool checkout durations from JSON logs.
Apply the included patch to **separate diagnostic build checkouts** of each
version with `git apply`; build both with `just bench-chat-build`. It enables
SQLx's otherwise-disabled pool timing. The patch hash is recorded. Leave the
primary latency binaries uninstrumented: debug logs themselves add work.
`usage_insert` includes server and network time; pool checkout includes liveness
checks and applies to every store, so it is not solely usage wait. Compare
matched `--usage-backend postgres` and `disabled` runs to isolate durable
accounting. Server execution time is separately recorded in `sql`.

`just bench-chat-copies` measures allocations for the old/new caller-side JSON
trace ownership and HTTP body construction. Parsing, writer/exporter internals,
cache retention and networking are outside its window. It asserts that the
adopted body keeps the raw JSON pointer. Its savings are narrower than whole
request allocation counts.

The load driver and collector share one process, and the mock runs one Uvicorn
worker. A driver near 100% CPU or a mock at its CPU ceiling limits the useful
throughput conclusions. This is a local diagnostic benchmark, not a production
capacity estimate or an open-loop tail-latency test. Increase duration and
repetitions before treating small differences or p99 as stable. Useful knobs:
`--duration`, `--warmup`, `--repeats`, `--concurrency`, `--prompt-words`,
`--model`, `--pool-size`, `--router-workers`, and `--usage-backend disabled`.
For sustained runs, `--control-duration 2` keeps direct controls short while
`--duration` sets the router measurement window.
`--min-successful 10000` rejects cells too short to exercise retention beyond
the 8,192-entry decision cache; use it for sustained outcome JSON/SSE cells.
Large-output cells can use `--model mock/l0-j0-t16384-e0-r0-c0` or
`mock/l0-j0-t131072-e0-r0-c0`, `--delivery json`, and a matching `--max-tokens`.
`response_bytes_p50` records the actual JSON wire size.

The harness creates a unique, labeled disposable database container, applies
migrations, starts its own mock/router/collector, then creates its provider
and keys through gRPC admin services. Sampling changes also use gRPC. All
resources belong to this run. In `finally` (including Ctrl-C/SIGTERM), it stops
children, saves Postgres diagnostics, truncates and verifies every app table,
and removes the database container. `cleanup.json` records the before/after
row counts and container removal. SIGKILL or host failure cannot run cleanup;
the container's `chalk-router-bench` label identifies any abandoned run.
Migrations run through `just migrate <frozen-binary>`. This optional recipe
argument uses the owned stack's final URL and both local-database guards,
without loading encrypted serving secrets. Normal `just migrate` is unchanged.

## Generated output

The output directory must be new. It contains run metadata, raw per-cell
`results.jsonl`, CSV/Markdown/JSON summaries, cleanup verification and logs.
Metadata records source/binary/mock hashes, the database image, scenario
settings and created provider IDs; raw keys and database credentials are omitted.
Generated `results/` directories are ignored by Git. Keep run evidence outside
the repository, for example attached to the pull request it supports.

Aggregate a completed run with:

```bash
uv run --project test_services/router_bench --locked python \
  test_services/router_bench/summarize.py /tmp/chat-bench-results
```

## Comparing runs

Compare two completed runs with:

```bash
uv run --project test_services/router_bench --locked python \
  test_services/router_bench/compare.py /tmp/chat-bench-before \
  /tmp/chat-bench-after /tmp/chat-bench-comparison
```

`--expect-outcome-retention` checks both HTTP-root and LLM retention for matching
keep-all outcomes. Enable it for the fixed implementation and leave it off when
measuring the original baseline's saturation behavior. The comparison checks
matching driver/mock/lock/image hashes, scenario settings, complete successful
measurements and exact durable accounting/cleanup before producing tables.

For key-only limits, use `--rate-scopes key`; `connection` applies connection-scoped limits
on that key and `both` retains the default two scopes. Budgets
remain attached to the API key. This separates first-admission consolidation
from the one-phase key-only path without changing load generation.
