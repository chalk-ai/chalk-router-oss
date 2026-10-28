"""Owned, disposable localhost stack; no existing router/database URL accepted."""

import argparse
import asyncio
import base64
import concurrent.futures
import csv
from datetime import UTC, datetime
import hashlib
import importlib
import json
import math
import os
from pathlib import Path
import platform
import random
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import uuid

import aiohttp
import grpc
import grpc_tools
import httpx
from opentelemetry.proto.collector.trace.v1 import trace_service_pb2 as otlp
import psycopg
from psycopg import sql

ROOT = Path(__file__).resolve().parents[2]
QUOTAS = ("disabled", "none", "rate", "budget", "both")
SAMPLING = ("disabled", "all_content", "all_metadata", "ten_percent", "drop", "outcome")
IMAGE = "postgres:16.14-alpine3.24@sha256:57c72fd2a128e416c7fcc499958864df5301e940bca0a56f58fddf30ffc07777"


def command(*args, **kwargs):
    return subprocess.check_output(args, text=True, **kwargs).strip()


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def cpu_seconds(pid):
    try:
        # Fields after comm (which may contain spaces) start at field 3.
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")
    except (OSError, ValueError):
        return None


def percentile(values, fraction):
    if not values:
        return None
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * fraction) - 1)]


def host_snapshot():
    return dict(load_average=os.getloadavg(), pressure={
        resource: Path(f"/proc/pressure/{resource}").read_text().strip()
        for resource in ("cpu", "io", "memory") if Path(f"/proc/pressure/{resource}").exists()
    })


def sql_profile(logs):
    """Diagnostic client timings, including network/server time; not CPU samples."""
    groups = {name: [] for name in ("pool_checkout", "usage_insert", "other_query")}
    for line in logs.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        fields = event.get("labels") or {}
        if event.get("target") == "sqlx::pool::acquire" and "acquired_after_secs" in fields:
            groups["pool_checkout"].append(float(fields["acquired_after_secs"]) * 1000)
        elif event.get("target") == "sqlx::query" and "elapsed_secs" in fields:
            statement = fields.get("db.statement", "") + fields.get("summary", "")
            group = "usage_insert" if "INSERT INTO chalk_router.usage_events" in statement else "other_query"
            groups[group].append(float(fields["elapsed_secs"]) * 1000)
    return {name: dict(count=len(values), sum_ms=sum(values),
                       p50_ms=percentile(values, 0.5), p95_ms=percentile(values, 0.95),
                       p99_ms=percentile(values, 0.99)) for name, values in groups.items()}


class Collector:
    """Real successful local OTLP exports, rather than a missing collector."""

    def __init__(self):
        self.lock = threading.Lock()
        self.spans = self.frontend_spans = self.bytes = 0
        self.root_spans = self.llm_spans = self.content_spans = 0
        self.server = grpc.server(concurrent.futures.ThreadPoolExecutor(max_workers=2),
                                  options=(("grpc.max_receive_message_length", 128 * 1024 * 1024),))
        handler = grpc.unary_unary_rpc_method_handler(
            self.export,
            request_deserializer=otlp.ExportTraceServiceRequest.FromString,
            response_serializer=otlp.ExportTraceServiceResponse.SerializeToString,
        )
        self.server.add_generic_rpc_handlers((grpc.method_handlers_generic_handler(
            "opentelemetry.proto.collector.trace.v1.TraceService", {"Export": handler}
        ),))
        self.port = self.server.add_insecure_port("127.0.0.1:0")
        self.server.start()

    def export(self, request, context):
        spans = [s for r in request.resource_spans for scope in r.scope_spans for s in scope.spans]
        with self.lock:
            self.spans += len(spans)
            self.frontend_spans += sum(s.name in ("chat.completion", "POST /v1/chat/completions") for s in spans)
            self.root_spans += sum(s.name == "POST /v1/chat/completions" for s in spans)
            self.llm_spans += sum(s.name == "chat.completion" for s in spans)
            self.content_spans += sum(any(a.key in ("input.value", "output.value") for a in s.attributes) for s in spans)
            self.bytes += request.ByteSize()
        return otlp.ExportTraceServiceResponse()

    def snapshot(self):
        with self.lock:
            return dict(spans=self.spans, frontend_spans=self.frontend_spans, bytes=self.bytes,
                        root_spans=self.root_spans, llm_spans=self.llm_spans, content_spans=self.content_spans)


class Stack:
    def __init__(self, args, output):
        self.args, self.output = args, output
        self.run_id = "chat-bench-" + uuid.uuid4().hex[:12]
        self.container = self.run_id
        self.env_id = self.run_id
        self.processes = []
        self.logs = []
        self.runtime_dir = tempfile.TemporaryDirectory(prefix="chat-bench-runtime-")
        self.router = self.mock = self.collector = self.db = None
        self.pg_started = False
        self.cleanup_result = {"verified": False}
        self.mock_port, self.router_port = free_port(), free_port()
        while self.router_port == self.mock_port:
            self.router_port = free_port()
        self.admin_key = uuid.uuid4().hex
        # Inherit only OS/tool settings; never serving credentials or dotenv.
        self.env = {k: v for k, v in os.environ.items() if k in (
            "PATH", "LANG", "LC_ALL", "TZ", "LD_LIBRARY_PATH", "SSL_CERT_FILE"
        )}

    def launch(self, name, argv, env=None):
        log = (self.output / f"{name}.log").open("a")
        self.logs.append(log)
        process = subprocess.Popen(argv, cwd=self.runtime_dir.name, env=env or self.env, stdout=log, stderr=log,
                                   start_new_session=True)
        self.processes.append(process)
        return process

    def stop(self, process):
        if process and process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                return
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()

    async def ready(self, url, process):
        async with httpx.AsyncClient(trust_env=False) as client:
            for _ in range(300):
                if process.poll() is not None:
                    raise RuntimeError(f"Process exited; see logs in {self.output}")
                try:
                    response = await client.get(url, timeout=1)
                    if response.is_success:
                        return
                except httpx.HTTPError:
                    pass
                await asyncio.sleep(0.1)
        raise TimeoutError(f"Not ready: {url}")

    async def start(self):
        command("docker", "run", "--detach", "--name", self.container,
                "--label", "chalk-router-bench=" + self.run_id,
                "--env", "POSTGRES_PASSWORD=bench", "--publish", "127.0.0.1::5432",
                self.args.postgres_image, "-c", "shared_preload_libraries=pg_stat_statements",
                "-c", "pg_stat_statements.track=all",
                "-c", "track_wal_io_timing=on")
        self.pg_started = True
        for _ in range(300):
            ready = subprocess.run(["docker", "exec", self.container, "pg_isready", "-h", "127.0.0.1", "-U", "postgres"],
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            if ready.returncode == 0:
                break
            await asyncio.sleep(0.1)
        else:
            raise TimeoutError("Disposable Postgres did not become ready over TCP")
        address = command("docker", "port", self.container, "5432/tcp")
        db_url = f"postgres://postgres:bench@127.0.0.1:{address.rsplit(':', 1)[1]}/postgres?sslmode=disable"
        self.db = psycopg.connect(db_url, autocommit=True)
        self.db.execute("CREATE EXTENSION pg_stat_statements")
        self.env.update(
            ROUTER_DATABASE_URL=db_url, CHALK_ENVIRONMENT_ID=self.env_id,
            ROUTER_LOCAL_DATABASE_URL=db_url,
            ROUTER_KEY_PEPPER=uuid.uuid4().hex,
            ROUTER_SECRETS_KEK=base64.b64encode(os.urandom(32)).decode(),
            ROUTER_ADMIN_KEY=self.admin_key, ROUTER_DANGEROUSLY_PERMIT_UNAUTHENTICATED="false",
            ROUTER_USAGE_BACKEND=self.args.usage_backend,
            ROUTER_DATABASE_MAX_CONNECTIONS=str(self.args.pool_size),
            PORT=str(self.router_port),
            RUST_LOG="warn,sqlx::pool::acquire=debug,sqlx::query=debug" if self.args.sql_profile else "warn",
            LOG_FORMAT="json",
            OTEL_LOG_LEVEL="info",
            OTEL_BSP_SCHEDULE_DELAY="100", OTEL_BSP_MAX_QUEUE_SIZE="65536",
            OTEL_BSP_MAX_EXPORT_BATCH_SIZE="512", TOKIO_WORKER_THREADS=str(self.args.router_workers),
        )
        migrated = self.launch("migrate", ["just", "--justfile", str(ROOT / "Justfile"),
                                          "--working-directory", self.runtime_dir.name,
                                          "migrate", str(self.args.binary)])
        await asyncio.to_thread(migrated.wait)
        if migrated.returncode:
            raise RuntimeError("Migration failed; see migrate.log")
        self.collector = Collector()
        self.env["OTEL_EXPORTER_OTLP_ENDPOINT"] = f"http://127.0.0.1:{self.collector.port}"
        self.mock = self.launch("mock", [sys.executable, "-m", "uvicorn", "app:app", "--app-dir",
                                str(ROOT / "test_services/mock_openai"), "--host", "127.0.0.1",
                                "--port", str(self.mock_port), "--workers", "1", "--no-access-log"])
        await self.ready(f"http://127.0.0.1:{self.mock_port}/health", self.mock)
        await self.restart_router("postgres")
        connection = await self.rpc("ProviderConnectionService", "CreateProviderConnection", dict(
            name=self.run_id, provider_kind="openai-compatible",
            base_url=f"http://127.0.0.1:{self.mock_port}/v1", prefix=self.run_id + "/",
            exposure=2, routing_enabled=True, api_key="unused",
        ))
        self.connection_id = connection.connection.id
        self.keys = {}
        for quota in QUOTAS:
            rows = []
            for index in range(self.args.keys):
                spec = dict(description=f"{self.run_id}-{quota}-{index}", permission=dict(
                    connection_scopes=[dict(connection_id=self.connection_id)],
                ))
                if quota in ("rate", "both"):
                    dimensions = dict(requests_per_minute=1_000_000_000)
                    if self.args.rate_dimensions == "all":
                        dimensions.update(tokens_per_minute=1_000_000_000, concurrent_requests=1_000_000)
                    spec["rate_limits"] = []
                    if self.args.rate_scopes in ("key", "both"):
                        spec["rate_limits"].append(dimensions)
                    if self.args.rate_scopes in ("connection", "both"):
                        spec["rate_limits"].append(dict(connection_id=self.connection_id, **dimensions))
                if quota in ("budget", "both"):
                    spec["usage_budgets"] = [dict(token_limit=1_000_000_000_000, period=1)]
                response = await self.rpc("ApiKeyService", "CreateKey", spec)
                rows.append(response.raw_key)
            self.keys[quota] = rows
        if self.args.quota_order == "interleaved":
            # Internal UUIDs can sort concurrency before token actions. Force
            # this legal ordering in the owned fixture, before runtime rows exist.
            assert self.db.execute("SELECT count(*) FROM chalk_router.quota_states").fetchone()[0] == 0
            rows = self.db.execute("""SELECT policy.id, rate.limit_type, policy.budget_limit_id
                FROM chalk_router.quota_policies policy
                LEFT JOIN chalk_router.rate_limits rate ON rate.id=policy.rate_limit_id
                  AND rate.environment_id=policy.environment_id
                WHERE policy.environment_id=%s""", (self.env_id,)).fetchall()
            with self.db.transaction():
                for policy_id, kind, budget in rows:
                    order = "0" if kind == "concurrent_requests" else "1" if budget or kind == "tokens_per_minute" else "2"
                    self.db.execute("UPDATE chalk_router.quota_policies SET id=%s WHERE id=%s",
                                    (order + ":" + policy_id, policy_id))
        self.metadata = dict(
            run_id=self.run_id, revision=command("git", "rev-parse", "HEAD", cwd=ROOT),
            binary_source_revision=self.args.binary_revision,
            captured_at_utc=datetime.now(UTC).isoformat(),
            dirty=bool(command("git", "status", "--porcelain", cwd=ROOT)),
            binary_sha256=hashlib.file_digest(self.args.binary.open("rb"), "sha256").hexdigest(),
            diagnostic_patch_sha256=hashlib.file_digest(self.args.diagnostic_patch.open("rb"), "sha256").hexdigest()
                if self.args.diagnostic_patch else None,
            driver_sha256=hashlib.file_digest(Path(__file__).open("rb"), "sha256").hexdigest(),
            lock_sha256=hashlib.file_digest((Path(__file__).parent / "uv.lock").open("rb"), "sha256").hexdigest(),
            load_client="aiohttp " + aiohttp.__version__,
            mock_sha256=hashlib.file_digest((ROOT / "test_services/mock_openai/app.py").open("rb"), "sha256").hexdigest(),
            postgres_image_id=command("docker", "inspect", "--format", "{{.Image}}", self.container),
            postgres_version=self.db.execute("SHOW server_version").fetchone()[0],
            python=sys.version, platform=platform.platform(), cpus=os.cpu_count(),
            affinity=sorted(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else None,
            cpu_max=Path("/sys/fs/cgroup/cpu.max").read_text().strip() if Path("/sys/fs/cgroup/cpu.max").exists() else None,
            connection_id=self.connection_id, environment_id=self.env_id,
            arguments={k: str(v) if isinstance(v, Path) else v for k, v in vars(self.args).items()},
            load="closed loop; persistent HTTP/1.1; concurrency workers; no retries",
        )

    async def restart_router(self, backend):
        self.stop(self.router)
        self.env["ROUTER_QUOTA_BACKEND"] = backend
        self.router = self.launch("router", [str(self.args.binary)])
        await self.ready(f"http://127.0.0.1:{self.router_port}/health", self.router)

    async def rpc(self, service, method, body):
        module_name = {
            "ProviderConnectionService": "provider_connection", "ApiKeyService": "api_key",
            "SamplingPolicyService": "sampling_policy",
        }[service]
        module = importlib.import_module(f"chalk.router.v1.{module_name}_pb2")
        request_type, response_type = getattr(module, method + "Request"), getattr(module, method + "Response")
        async with grpc.aio.insecure_channel(f"127.0.0.1:{self.router_port}") as channel:
            call = channel.unary_unary(f"/chalk.router.v1.{service}/{method}",
                                      request_serializer=request_type.SerializeToString,
                                      response_deserializer=response_type.FromString)
            return await call(request_type(**body), metadata=(("authorization", "Bearer " + self.admin_key),), timeout=10)

    async def sampling(self, name):
        update = dict(enabled=name != "disabled", default_rate=1.0, min_conversation_rate=0.0,
                      default_log_content=name in ("disabled", "all_content", "ten_percent", "drop_content"),
                      rules=[], outcome_rules=[])
        if name == "ten_percent":
            update["default_rate"] = 0.1
        elif name in ("drop", "drop_content"):
            update["default_rate"] = 0.0
        elif name in ("outcome", "outcome_all"):
            update["default_rate"] = 0.1 if name == "outcome" else 1.0
            update["outcome_rules"] = [dict(path="choices.0.finish_reason", op=1,
                                            value=dict(string_value="stop"), rate=1.0)]
        elif name == "rules_1000":
            update["rules"] = [dict(request_tag=f"unmatched-{i}", rate=0.1) for i in range(1000)]
        await self.rpc("SamplingPolicyService", "UpdateSamplingPolicy", dict(
            update=update, update_mask=dict(paths=list(update)),
        ))

    def queries(self):
        rows = self.db.execute("""SELECT query, calls, total_exec_time, rows, toplevel
            FROM pg_stat_statements WHERE userid = (SELECT oid FROM pg_roles WHERE rolname='postgres')
            AND query NOT LIKE '%%pg_stat_statements%%' ORDER BY total_exec_time DESC""").fetchall()
        return [dict(query=q, calls=n, exec_ms=ms, rows=r, toplevel=top) for q, n, ms, r, top in rows]

    def deadlocks(self):
        return self.db.execute("SELECT deadlocks FROM pg_stat_database WHERE datname=current_database()").fetchone()[0]

    def wal(self):
        names = ("wal_records", "wal_fpi", "wal_bytes", "wal_buffers_full", "wal_write", "wal_sync", "wal_write_time", "wal_sync_time")
        row = self.db.execute("SELECT " + ",".join(names) + " FROM pg_stat_wal").fetchone()
        return {name: float(value) for name, value in zip(names, row)}

    def close(self):
        for process in reversed(self.processes):
            self.stop(process)
        if self.collector:
            self.collector.server.stop(1).wait()
        cleanup_errors = []
        try:
            self.clear_database()
        except Exception as error:
            cleanup_errors.append(str(error))
        try:
            if self.pg_started:
                with (self.output / "postgres.log").open("w") as log:
                    subprocess.run(["docker", "logs", self.container], stdout=log, stderr=log)
                # Unique generated name; never remove any preexisting container.
                subprocess.run(["docker", "rm", "--force", "--volumes", self.container], check=True, stdout=subprocess.DEVNULL)
                self.cleanup_result["container_removed"] = True
        except Exception as error:
            cleanup_errors.append(str(error))
        finally:
            if self.db:
                self.db.close()
            for log in self.logs:
                log.close()
            self.runtime_dir.cleanup()
        if cleanup_errors:
            self.cleanup_result["errors"] = cleanup_errors
            raise RuntimeError(f"Cleanup failed: {cleanup_errors}")

    def clear_database(self):
        if self.db:
            # Inspect every app table, including runtime rows without an environment column.
            tables = [row[0] for row in self.db.execute("""SELECT tablename FROM pg_tables
                       WHERE schemaname='chalk_router' ORDER BY tablename""")]
            before = {table: self.db.execute(sql.SQL("SELECT count(*) FROM chalk_router.{}").format(
                sql.Identifier(table))).fetchone()[0] for table in tables}
            with self.db.transaction():
                self.db.execute(sql.SQL("TRUNCATE {} CASCADE").format(sql.SQL(", ").join(
                    sql.Identifier("chalk_router", table) for table in tables)))
            after = {table: self.db.execute(sql.SQL("SELECT count(*) FROM chalk_router.{}").format(
                sql.Identifier(table))).fetchone()[0] for table in tables}
            self.cleanup_result = dict(verified=all(n == 0 for n in after.values()), before=before, after=after)


async def load(stack, cell, duration):
    direct = cell["quota"] == "direct"
    url = f"http://127.0.0.1:{stack.mock_port if direct else stack.router_port}/v1/chat/completions"
    model = stack.args.model if direct else stack.run_id + "/" + stack.args.model
    messages = [dict(role="user", content="hello " * max(1, stack.args.prompt_words))]
    body = dict(model=model, messages=messages, max_tokens=stack.args.max_tokens, stream=cell["stream"])
    if cell["stream"]:
        body["stream_options"] = dict(include_usage=True)
    tokens = json.dumps(messages)[:200].split()
    expected_prompt = len(tokens)
    records, statuses, errors, response_bytes = [], {}, [], []
    start = time.perf_counter()
    deadline = start + duration
    connector = aiohttp.TCPConnector(limit=cell["concurrency"], limit_per_host=cell["concurrency"])
    async with aiohttp.ClientSession(connector=connector, trust_env=False,
                                     timeout=aiohttp.ClientTimeout(total=30)) as client:
        async def worker(index):
            headers = {} if direct else {"authorization": "Bearer " + stack.keys[cell["quota"]][index % stack.args.keys]}
            while time.perf_counter() < deadline:
                begun = time.perf_counter()
                first = None
                status = "transport_error"
                try:
                    async with client.post(url, json=body, headers=headers) as response:
                        status = str(response.status)
                        header_ms = (time.perf_counter() - begun) * 1000
                        usage, finished, done, content, buffer = None, False, False, False, ""
                        if cell["stream"] and response.status == 200:
                            async for data_chunk in response.content.iter_any():
                                chunk = data_chunk.decode()
                                buffer += chunk.replace("\r\n", "\n")
                                while "\n\n" in buffer:
                                    frame, buffer = buffer.split("\n\n", 1)
                                    for line in frame.splitlines():
                                        if not line.startswith("data:"):
                                            continue
                                        data = line[5:].strip()
                                        if data == "[DONE]":
                                            done = True
                                            continue
                                        event = json.loads(data)
                                        if "error" in event:
                                            raise ValueError(str(event["error"]))
                                        usage = event.get("usage") or usage
                                        for choice in event.get("choices", []):
                                            if choice.get("delta", {}).get("content"):
                                                content = True
                                                if first is None:
                                                    first = (time.perf_counter() - begun) * 1000
                                            finished |= choice.get("finish_reason") == "stop"
                        else:
                            raw = await response.read()
                            response_bytes.append(len(raw))
                            if response.status != 200:
                                raise ValueError(f"HTTP {status}: {raw[:300]!r}")
                            parsed = json.loads(raw)
                            usage = parsed.get("usage")
                            content = bool(parsed["choices"][0]["message"].get("content"))
                            finished = parsed["choices"][0].get("finish_reason") == "stop"
                            done = True
                        elapsed = (time.perf_counter() - begun) * 1000
                        if not (done and finished and content and usage):
                            raise ValueError("Missing terminal marker, finish reason, content or usage")
                        if usage["prompt_tokens"] != expected_prompt or usage["completion_tokens"] <= 0:
                            raise ValueError("Unexpected mock usage")
                        if usage["total_tokens"] != usage["prompt_tokens"] + usage["completion_tokens"]:
                            raise ValueError("Inconsistent total usage")
                        records.append((elapsed, first, header_ms))
                except (aiohttp.ClientError, TimeoutError, ValueError, KeyError) as error:
                    if len(errors) < 5:
                        errors.append(str(error))
                    status += "_invalid" if status == "200" else ""
                statuses[status] = statuses.get(status, 0) + 1
        await asyncio.gather(*(worker(i) for i in range(cell["concurrency"])))
    wall = time.perf_counter() - start
    totals = [r[0] for r in records]
    ttft = [r[1] for r in records if r[1] is not None]
    return dict(requests=sum(statuses.values()), successful=len(records), statuses=statuses, errors=errors,
                wall_s=wall, rps=len(records) / wall, mean_ms=sum(totals) / len(totals) if totals else None,
                p50_ms=percentile(totals, 0.5), p95_ms=percentile(totals, 0.95), p99_ms=percentile(totals, 0.99),
                ttft_p50_ms=percentile(ttft, 0.5), ttft_p95_ms=percentile(ttft, 0.95),
                headers_p50_ms=percentile([r[2] for r in records], 0.5),
                response_bytes_p50=percentile(response_bytes, 0.5))


async def run(args):
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    stack = Stack(args, output)
    results = []
    try:
        await stack.start()
        (output / "metadata.json").write_text(json.dumps(stack.metadata, indent=2) + "\n")
        rng = random.Random(args.seed)
        active_sampling = None
        for repeat in range(args.repeats):
            # Baseline is measured on both sides of each repetition for drift.
            for position in ("before", "matrix", "after"):
                if position != "matrix":
                    cells = [dict(quota="direct", sampling="direct", concurrency=c, stream=s,
                                  repeat=repeat, position=position) for c in args.concurrency for s in args.streams]
                else:
                    # Two process groups; randomize their order and all cells within each.
                    cells = []
                    groups = [[q for q in args.quotas if q == "disabled"],
                              [q for q in args.quotas if q != "disabled"]]
                    rng.shuffle(groups)
                    for group in groups:
                        policies = list(args.sampling)
                        rng.shuffle(policies)
                        for policy in policies:
                            group_cells = [dict(quota=q, sampling=policy, concurrency=c, stream=s,
                                                repeat=repeat, position=position) for q in group
                                           for c in args.concurrency for s in args.streams]
                            rng.shuffle(group_cells)
                            cells.extend(group_cells)
                for cell in cells:
                    started_at = datetime.now(UTC).isoformat()
                    if cell["quota"] != "direct":
                        backend = "disabled" if cell["quota"] == "disabled" else "postgres"
                        if active_sampling != (backend, cell["sampling"]):
                            await stack.sampling(cell["sampling"])
                            # Serving policy refresh has a five-second TTL; boot primes
                            # the persisted policy synchronously before accepting requests.
                            await stack.restart_router(backend)
                            active_sampling = (backend, cell["sampling"])
                    log_path = output / "router.log"
                    log_offset = log_path.stat().st_size
                    deadlocks_before = stack.deadlocks()
                    await load(stack, cell, args.warmup)
                    await asyncio.sleep(0.3)  # Drain previous export batches before counter snapshots.
                    log_offset = log_path.stat().st_size
                    stack.db.execute("SELECT pg_stat_statements_reset()")
                    usage_before = stack.db.execute("SELECT count(*) FROM chalk_router.usage_events").fetchone()[0]
                    wal_before = stack.wal()
                    host_before = host_snapshot()
                    cpu_before = [cpu_seconds(p) for p in (stack.router.pid, stack.mock.pid, os.getpid())]
                    exports_before = stack.collector.snapshot()
                    duration = args.control_duration if cell["quota"] == "direct" and args.control_duration is not None else args.duration
                    result = await load(stack, cell, duration)
                    cpu_after = [cpu_seconds(p) for p in (stack.router.pid, stack.mock.pid, os.getpid())]
                    result["cpu_pct"] = {name: (b - a) / result["wall_s"] * 100 if a is not None and b is not None else None
                                         for name, a, b in zip(("router", "mock", "driver"), cpu_before, cpu_after)}
                    result["sql"] = stack.queries()
                    result["usage_rows"] = stack.db.execute("SELECT count(*) FROM chalk_router.usage_events").fetchone()[0] - usage_before
                    await asyncio.sleep(0.3)
                    result["host"] = dict(before=host_before, after=host_snapshot())
                    result["wal"] = {k: v - wal_before[k] for k, v in stack.wal().items()}
                    exports_after = stack.collector.snapshot()
                    result["exports"] = {k: exports_after[k] - exports_before[k] for k in exports_before}
                    sampling_error = None
                    if cell["quota"] != "direct":
                        if cell["sampling"] in ("all_metadata", "drop", "outcome", "outcome_all", "rules_1000") and result["exports"]["content_spans"]:
                            sampling_error = "Sampling content policy did not converge"
                        if cell["sampling"] in ("disabled", "all_content", "all_metadata", "outcome_all", "rules_1000"):
                            expected_spans = 2 * result["successful"]
                            if abs(result["exports"]["frontend_spans"] - expected_spans) > max(4, expected_spans * 0.02):
                                sampling_error = "Unexpected export count under a keep-all policy"
                        if cell["sampling"] in ("drop", "drop_content") and result["exports"]["frontend_spans"]:
                            sampling_error = "Unexpected exports under a drop-all policy"
                        if cell["sampling"] in ("outcome", "outcome_all") and args.expect_outcome_retention:
                            for span_kind in ("root_spans", "llm_spans"):
                                if abs(result["exports"][span_kind] - result["successful"]) > max(4, result["successful"] * 0.02):
                                    sampling_error = "Outcome retention did not cover both HTTP and LLM spans"
                    result["sampling_error"] = sampling_error
                    with log_path.open() as log:
                        log.seek(log_offset)
                        logs = log.read()
                    if args.sql_profile:
                        result["sql_profile"] = sql_profile(logs)
                        if cell["quota"] != "direct":
                            if not result["sql_profile"]["pool_checkout"]["count"]:
                                raise RuntimeError("Missing diagnostic pool timing; apply the pool patch to this binary")
                            if args.usage_backend == "postgres" and result["sql_profile"]["usage_insert"]["count"] != result["successful"]:
                                raise RuntimeError("Diagnostic usage INSERT timings do not match request count")
                    result["backend_errors"] = {message: logs.count(message) for message in (
                        "quota acquisition failed; allowing request", "quota reconciliation failed",
                        "quota backend database error", "quota backend pool exhausted",
                        "quota admission lock wait timed out", "Durable usage write failed",
                        "Retrying durable terminal accounting",
                        "Combined quota accounting failed",
                        "OpenTelemetry trace error",
                    )}
                    result["deadlocks"] = stack.deadlocks() - deadlocks_before
                    result["healthy"] = not (any(result["backend_errors"].values()) or result["deadlocks"]
                                               or result["errors"] or not result["successful"] or sampling_error)
                    result.update(cell)
                    result["started_at_utc"] = started_at
                    result["keys"] = args.keys
                    results.append(result)
                    with (output / "results.jsonl").open("a") as file:
                        file.write(json.dumps(result) + "\n")
                    print(f"{repeat+1}/{args.repeats} {cell['quota']:8} {cell['sampling']:12} "
                          f"c={cell['concurrency']:2} stream={int(cell['stream'])} "
                          f"{result['rps']:7.1f} req/s p95={result['p95_ms']} "
                          f"errors={result['requests']-result['successful']} healthy={result['healthy']}", flush=True)
                    if sampling_error:
                        raise RuntimeError(sampling_error)
                    if result["successful"] != result["requests"] or not result["successful"]:
                        raise RuntimeError(f"Invalid benchmark cell: {result['errors']}")
                    if cell["quota"] != "direct" and result["successful"] < args.min_successful:
                        raise RuntimeError("Insufficient requests for the sustained retention check")
                    if args.usage_backend == "postgres" and cell["quota"] != "direct" and result["usage_rows"] != result["successful"]:
                        raise RuntimeError("Durable usage count does not match successful requests")
    finally:
        try:
            stack.close()
        finally:
            (output / "cleanup.json").write_text(json.dumps(stack.cleanup_result, indent=2) + "\n")
            if results:
                columns = [k for k in results[0] if k not in ("sql", "errors", "statuses", "exports", "cpu_pct", "backend_errors", "sql_profile", "host", "wal")]
                with (output / "results.csv").open("w") as file:
                    writer = csv.DictWriter(file, columns, extrasaction="ignore")
                    writer.writeheader()
                    writer.writerows(results)
    if any(not row["healthy"] for row in results):
        raise RuntimeError("Backend errors detected; unhealthy cells are diagnostic only. See results and logs.")


def generate_protos(directory):
    """Generate protobuf clients in a temporary directory; never edit ui/src/gen.
    Router protos import the vendored chalk.auth and chalk.utils packages."""
    include = str(Path(grpc_tools.__file__).parent / "_proto")
    command(sys.executable, "-m", "grpc_tools.protoc", "-I" + str(ROOT / "proto"), "-I" + include,
            "--python_out=" + directory,
            *(str(path) for package in ("auth", "utils", "router")
              for path in sorted((ROOT / "proto/chalk" / package).rglob("*.proto"))))


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/openai-server")
    parser.add_argument("--binary-revision", help="Source commit for the binary, especially when benchmarking another worktree")
    parser.add_argument("--output", type=Path, required=True, help="New output directory; refuses to overwrite")
    parser.add_argument("--postgres-image", default=IMAGE)
    parser.add_argument("--duration", type=float, default=3)
    parser.add_argument("--control-duration", type=float, help="Optional direct-control duration for longer targeted router cells")
    parser.add_argument("--warmup", type=float, default=0.5)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--concurrency", type=int, nargs="+", default=[1, 16])
    parser.add_argument("--quotas", nargs="+", choices=QUOTAS, default=list(QUOTAS))
    parser.add_argument("--sampling", nargs="+", choices=(*SAMPLING, "drop_content", "rules_1000", "outcome_all"), default=list(SAMPLING))
    parser.add_argument("--sql-profile", action="store_true",
                        help="Enable diagnostic SQL logs; requires the same pool-timing patch in both diagnostic binaries")
    parser.add_argument("--diagnostic-patch", type=Path, help="Applied diagnostic source patch; hash recorded in provenance")
    parser.add_argument("--expect-outcome-retention", action="store_true",
                        help="Require the matching keep-all outcome to retain both HTTP and LLM spans")
    parser.add_argument("--delivery", nargs="+", choices=("json", "sse"), default=["json", "sse"])
    parser.add_argument("--model", default="mock/l0-j0-t8-e0-r0-c0")
    parser.add_argument("--prompt-words", type=int, default=16)
    parser.add_argument("--max-tokens", type=int, default=64)
    parser.add_argument("--min-successful", type=int, default=0, help="Minimum measured requests per router cell, e.g. 10000 for sustained outcome retention")
    parser.add_argument("--keys", type=int, default=1, help="Round-robin worker identities; 1 measures shared-key contention")
    parser.add_argument("--rate-dimensions", choices=("requests", "all"), default="requests",
                        help="Global + route RPM; all also adds TPM and concurrency to both scopes")
    parser.add_argument("--rate-scopes", choices=("key", "connection", "both"), default="both",
                        help="Apply rate definitions to the API key, its connection, or both")
    parser.add_argument("--quota-order", choices=("random", "interleaved"), default="random",
                        help="Interleaved remaps internal bridge IDs before traffic to reproduce opposing lock orders")
    parser.add_argument("--pool-size", type=int, default=20)
    parser.add_argument("--router-workers", type=int, default=4)
    parser.add_argument("--usage-backend", choices=("postgres", "disabled"), default="postgres")
    parser.add_argument("--seed", type=int, default=1729)
    args = parser.parse_args()
    if args.sql_profile != bool(args.diagnostic_patch):
        parser.error("--sql-profile and --diagnostic-patch must be used together")
    for name in ("duration", "warmup", "repeats", "prompt_words", "max_tokens", "keys", "pool_size", "router_workers"):
        if getattr(args, name) <= 0:
            parser.error(f"--{name.replace('_', '-')} must be positive")
    if any(c <= 0 for c in args.concurrency):
        parser.error("concurrency must be positive")
    if args.control_duration is not None and args.control_duration <= 0:
        parser.error("--control-duration must be positive")
    args.binary = args.binary.resolve()
    if not args.binary.is_file():
        parser.error("Build first: just bench-chat-build")
    args.streams = [d == "sse" for d in args.delivery]
    return args


if __name__ == "__main__":
    args = parse_args()
    with tempfile.TemporaryDirectory(prefix="chat-bench-proto-") as generated:
        generate_protos(generated)
        sys.path.insert(0, generated)
        # SIGTERM follows the same finally cleanup path as Ctrl-C.
        signal.signal(signal.SIGTERM, lambda signum, frame: (_ for _ in ()).throw(KeyboardInterrupt()))
        asyncio.run(run(args))
