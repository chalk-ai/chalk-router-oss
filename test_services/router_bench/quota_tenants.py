"""Multi-tenant quota admission benchmark on owned, disposable localhost stacks.

Compares router binaries over scenarios that separate the simple path from
shared-row contention and from many independent tenants:

  simple   keys with no quota definitions (latency floor)
  hot      one key with key + connection RPM/TPM/concurrency and a daily budget
  tenants  N keys in M usage pools; every key and pool has key/pool +
           connection RPM/TPM/concurrency and a daily budget, so keys share
           their pool's rows
  indep    N keys, each in its own pool with the same definitions (no rows
           shared between requests on different keys)

Every scenario runs at each `--db-delay-ms`, injected as a netem egress delay
inside the Postgres container's network namespace to model a remote database
(needs passwordless sudo, `nsenter` and `tc`). Versions alternate order across
repeats; each (version, repeat) gets a fresh stack. After every cell the
database must show one usage row per success and no residual receipts, leases
or active counts; any non-200 or backend error fails the run.
"""

import argparse
import asyncio
import importlib
import json
from pathlib import Path
import statistics
import subprocess
import sys
import tempfile
import time

import aiohttp
import grpc
import grpc_tools

import bench

LIMIT = 1_000_000_000
SCENARIOS = ("simple", "hot", "tenants", "indep")
DEFAULT_CELLS = ("simple:1", "simple:64", "hot:1", "hot:16", "hot:64",
                 "tenants:64", "tenants:128", "indep:1", "indep:64", "indep:128")
BACKEND_ERRORS = (
    "request quota acquisition failed; allowing request", "route quota acquisition failed; allowing request",
    "quota backend database error", "quota backend pool exhausted", "quota admission lock wait timed out",
    "Durable usage write failed", "Combined quota accounting failed",
)


def dimensions():
    return dict(requests_per_minute=LIMIT, tokens_per_minute=LIMIT, concurrent_requests=1_000_000)


class TenantStack(bench.Stack):
    async def admin(self, service, module_name, method, body):
        module = importlib.import_module(f"chalk.router.v1.{module_name}_pb2")
        request, response = getattr(module, method + "Request"), getattr(module, method + "Response")
        async with grpc.aio.insecure_channel(f"127.0.0.1:{self.router_port}") as channel:
            call = channel.unary_unary(f"/chalk.router.v1.{service}/{method}",
                                      request_serializer=request.SerializeToString,
                                      response_deserializer=response.FromString)
            return await call(request(**body), metadata=(("authorization", "Bearer " + self.admin_key),), timeout=30)

    async def provision(self, tenants, pools):
        permission = dict(connection_scopes=[dict(connection_id=self.connection_id)])
        limits = [dimensions(), dict(connection_id=self.connection_id, **dimensions())]
        budgets = [dict(token_limit=LIMIT * 1000, period=1)]

        async def key(description, **spec):
            created = await self.admin("ApiKeyService", "api_key", "CreateKey",
                                       dict(description=description, permission=permission, **spec))
            return created.raw_key

        self.tenant_keys = dict(hot=[await key("hot", rate_limits=limits, usage_budgets=budgets)])
        self.tenant_keys["simple"] = [await key(f"simple-{i}") for i in range(tenants)]
        for scenario, pool_count in (("tenants", pools), ("indep", tenants)):
            pool_ids = []
            for index in range(pool_count):
                created = await self.admin("UsagePoolService", "usage_pool", "CreateUsagePool", dict(
                    name=f"{self.run_id}-{scenario}-{index}", rate_limits=limits, usage_budgets=budgets))
                pool_ids.append(created.pool.id)
            self.tenant_keys[scenario] = [
                await key(f"{scenario}-{i}", usage_pool_id=pool_ids[i % pool_count],
                          rate_limits=limits, usage_budgets=budgets)
                for i in range(tenants)
            ]

    def delay(self, one_way_ms):
        pid = bench.command("docker", "inspect", "-f", "{{.State.Pid}}", self.container)
        netns = ["sudo", "-n", "nsenter", "-t", pid, "-n", "tc", "qdisc"]
        subprocess.run(netns + ["del", "dev", "eth0", "root"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if one_way_ms:
            subprocess.run(netns + ["add", "dev", "eth0", "root", "netem", "delay", f"{one_way_ms}ms"], check=True)

    def runtime_rows(self):
        def scalar(query):
            return int(self.db.execute(query).fetchone()[0])
        return dict(
            reservations=scalar("SELECT count(*) FROM chalk_router.quota_reservations"),
            leases=scalar("SELECT count(*) FROM chalk_router.quota_leases"),
            active=scalar("SELECT coalesce(sum(active_count), 0) FROM chalk_router.quota_states"),
            usage=scalar("SELECT count(*) FROM chalk_router.usage_events"),
        )


async def load(stack, scenario, concurrency, duration):
    url = f"http://127.0.0.1:{stack.router_port}/v1/chat/completions"
    body = dict(model=stack.run_id + "/" + stack.args.model,
                messages=[dict(role="user", content="hello " * stack.args.prompt_words)],
                max_tokens=stack.args.max_tokens, stream=False)
    keys = stack.tenant_keys[scenario]
    latencies, statuses, errors = [], {}, []
    start = time.perf_counter()
    deadline = start + duration
    connector = aiohttp.TCPConnector(limit=concurrency, limit_per_host=concurrency)
    async with aiohttp.ClientSession(connector=connector, trust_env=False,
                                     timeout=aiohttp.ClientTimeout(total=60)) as client:
        async def worker(index):
            headers = {"authorization": "Bearer " + keys[index % len(keys)]}
            while time.perf_counter() < deadline:
                begun = time.perf_counter()
                status = "transport_error"
                try:
                    async with client.post(url, json=body, headers=headers) as response:
                        status = str(response.status)
                        raw = await response.read()
                        if response.status != 200:
                            raise ValueError(f"HTTP {status}: {raw[:200]!r}")
                        if json.loads(raw)["usage"]["total_tokens"] <= 0:
                            raise ValueError("missing usage")
                        latencies.append((time.perf_counter() - begun) * 1000)
                except (aiohttp.ClientError, TimeoutError, ValueError, KeyError) as error:
                    if len(errors) < 5:
                        errors.append(str(error))
                statuses[status] = statuses.get(status, 0) + 1
        await asyncio.gather(*(worker(i) for i in range(concurrency)))
    wall = time.perf_counter() - start
    return dict(requests=sum(statuses.values()), successful=len(latencies), statuses=statuses, errors=errors,
                rps=len(latencies) / wall, p50_ms=bench.percentile(latencies, 0.5),
                p95_ms=bench.percentile(latencies, 0.95), p99_ms=bench.percentile(latencies, 0.99))


async def run_stack(args, label, binary, repeat, output):
    args.binary, args.binary_revision = binary, label
    stack = TenantStack(args, output)
    rows = []
    try:
        await stack.start()
        (output / "metadata.json").write_text(json.dumps(stack.metadata, indent=2, default=str) + "\n")
        await stack.sampling("all_metadata")
        await stack.restart_router("postgres")
        await stack.provision(args.tenants, args.pools)
        for delay in args.db_delay_ms:
            stack.delay(delay)
            for scenario, concurrency in args.cells:
                await load(stack, scenario, concurrency, args.warmup)
                await asyncio.sleep(0.5)
                before, deadlocks = stack.runtime_rows(), stack.deadlocks()
                cpu = bench.cpu_seconds(stack.router.pid)
                result = await load(stack, scenario, concurrency, args.duration)
                cpu = bench.cpu_seconds(stack.router.pid) - cpu
                await asyncio.sleep(0.5)  # Let terminal accounting finish before auditing.
                after = stack.runtime_rows()
                result.update(version=label, repeat=repeat, scenario=scenario, concurrency=concurrency,
                              db_delay_ms=delay, deadlocks=stack.deadlocks() - deadlocks,
                              router_cpu_ms_per_request=cpu * 1000 / max(1, result["successful"]),
                              usage_rows=after["usage"] - before["usage"],
                              residual={k: after[k] for k in ("reservations", "leases", "active")})
                result["consistent"] = (result["successful"] == result["requests"] == result["usage_rows"]
                                        and not any(result["residual"].values()) and not result["deadlocks"])
                rows.append(result)
                with (output / "results.jsonl").open("a") as file:
                    file.write(json.dumps(result) + "\n")
                print(f"{label} r{repeat} delay={delay}ms {scenario:8} c={concurrency:3} {result['rps']:8.1f} req/s "
                      f"p50={result['p50_ms'] or 0:.2f} p99={result['p99_ms'] or 0:.2f} ok={result['consistent']}",
                      flush=True)
                if not result["consistent"]:
                    raise RuntimeError(f"Inconsistent cell: {result['statuses']} {result['errors']} "
                                       f"usage={result['usage_rows']} residual={result['residual']}")
        stack.delay(0)
        logs = (output / "router.log").read_text()
        errors = {message: logs.count(message) for message in BACKEND_ERRORS}
        if any(errors.values()):
            raise RuntimeError(f"Backend errors: {errors}")
    finally:
        try:
            stack.close()
        finally:
            (output / "cleanup.json").write_text(json.dumps(stack.cleanup_result, indent=2) + "\n")
    if not stack.cleanup_result.get("verified"):
        raise RuntimeError(f"Owned database was not verified empty; see {output / 'cleanup.json'}")
    return rows


def summarize(rows, labels):
    def median(version, key, metric):
        values = [r[metric] for r in rows if r["version"] == version
                  and (r["db_delay_ms"], r["scenario"], r["concurrency"]) == key]
        return statistics.median(values) if values else None

    base, *others = labels
    keys = sorted({(r["db_delay_ms"], r["scenario"], r["concurrency"]) for r in rows},
                  key=lambda k: (k[0], SCENARIOS.index(k[1]), k[2]))
    lines = ["| DB delay | Scenario | Concurrency | " + " | ".join(f"{v} req/s" for v in labels)
             + " | " + " | ".join(f"{v} vs {base}" for v in others)
             + " | " + " | ".join(f"{v} p50 / p99 ms" for v in labels) + " |",
             "| --- | --- | ---: |" + " ---: |" * (2 * len(labels) + len(others))]
    for key in keys:
        rps = [median(v, key, "rps") for v in labels]
        change = [f"{100 * (r / rps[0] - 1):+.1f}%" for r in rps[1:]]
        latency = [f"{median(v, key, 'p50_ms'):.2f} / {median(v, key, 'p99_ms'):.2f}" for v in labels]
        lines.append(f"| {key[0]:g} ms | {key[1]} | {key[2]} | " + " | ".join(f"{r:.1f}" for r in rps)
                     + " | " + " | ".join(change) + " | " + " | ".join(latency) + " |")
    return "\n".join(lines)


async def run(args):
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    rows = []
    for repeat in range(args.repeats):
        # Alternate version order so drift and warm caches do not favor one side.
        order = args.binaries if repeat % 2 == 0 else list(reversed(args.binaries))
        for label, binary in order:
            stack_output = output / f"{label}-r{repeat}"
            stack_output.mkdir()
            rows.extend(await run_stack(args, label, binary, repeat, stack_output))
    table = summarize(rows, [label for label, _ in args.binaries])
    (output / "summary.md").write_text(table + "\n")
    print(table)


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", action="append", required=True, metavar="LABEL=PATH",
                        help="Router binary to measure; repeat to compare (the first is the baseline)")
    parser.add_argument("--output", type=Path, required=True, help="New output directory; refuses to overwrite")
    parser.add_argument("--postgres-image", default=bench.IMAGE)
    parser.add_argument("--duration", type=float, default=15)
    parser.add_argument("--warmup", type=float, default=3)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--cells", nargs="+", default=list(DEFAULT_CELLS), metavar="SCENARIO:CONCURRENCY")
    parser.add_argument("--tenants", type=int, default=64, help="Keys in the tenants and indep scenarios")
    parser.add_argument("--pools", type=int, default=8, help="Usage pools shared by the tenants scenario's keys")
    parser.add_argument("--db-delay-ms", type=float, nargs="+", default=[0, 1],
                        help="One-way Postgres delay per pass; nonzero values need passwordless sudo")
    parser.add_argument("--pool-size", type=int, default=20)
    parser.add_argument("--router-workers", type=int, default=4)
    parser.add_argument("--model", default="mock/l0-j0-t8-e0-r0-c0")
    parser.add_argument("--prompt-words", type=int, default=16)
    parser.add_argument("--max-tokens", type=int, default=64)
    args = parser.parse_args()
    args.binaries = []
    for value in args.binary:
        label, _, path = value.partition("=")
        binary = Path(path).resolve()
        if not label or not binary.is_file():
            parser.error(f"--binary expects LABEL=PATH to a built router: {value}")
        args.binaries.append((label, binary))
    if len({label for label, _ in args.binaries}) != len(args.binaries):
        parser.error("binary labels must be unique")
    try:
        args.cells = [(scenario, int(concurrency)) for scenario, _, concurrency in
                      (cell.partition(":") for cell in args.cells)]
    except ValueError:
        parser.error("--cells expects SCENARIO:CONCURRENCY")
    if any(scenario not in SCENARIOS or concurrency <= 0 for scenario, concurrency in args.cells):
        parser.error(f"cell scenarios must be one of {SCENARIOS} with positive concurrency")
    if any(args.db_delay_ms) and subprocess.run(["sudo", "-n", "true"], capture_output=True).returncode:
        parser.error("nonzero --db-delay-ms needs passwordless sudo for nsenter/tc; pass --db-delay-ms 0")
    if args.tenants <= 0 or args.pools <= 0 or args.pools > args.tenants:
        parser.error("--tenants and --pools must be positive, with pools <= tenants")
    # Fields the shared stack reads; this driver owns its own load and keys.
    args.usage_backend, args.sql_profile, args.diagnostic_patch = "postgres", False, None
    args.keys, args.rate_dimensions, args.rate_scopes, args.quota_order = 1, "requests", "both", "random"
    return args


if __name__ == "__main__":
    args = parse_args()
    with tempfile.TemporaryDirectory(prefix="quota-tenants-proto-") as generated:
        bench.generate_protos(generated)
        sys.path.insert(0, generated)
        asyncio.run(run(args))
