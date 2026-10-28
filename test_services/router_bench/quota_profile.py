"""CPU and Postgres wait diagnostics using bench.py's disposable stack.

Run through the benchmark's locked uv project with normal bench.py arguments.
Requires sudo -n perf. Profiles only measured router cells; warmup and direct
controls are excluded. Profiling overhead makes these diagnostic timings.
"""

import asyncio
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time

import grpc_tools
import psycopg

import bench


def query_kind(query):
    if "acquire_quota" in query or "acquire_request_and_route_quota" in query:
        return "acquire"
    if "finish_quota" in query:
        return "finish"
    if "INSERT INTO chalk_router.usage_events" in query:
        return "usage_insert"
    if "FROM chalk_router.api_keys" in query:
        return "authenticate"
    return query.split(None, 1)[0].upper() if query.strip() else "other"


def observe_database(dsn, stop, observations):
    try:
        with psycopg.connect(dsn, autocommit=True,
                             application_name="quota-profile-observer") as connection:
            while not stop.is_set():
                started = time.monotonic()
                rows = connection.execute("""SELECT query, wait_event_type, wait_event
                    FROM pg_stat_activity WHERE backend_type='client backend'
                    AND state='active' AND pid <> pg_backend_pid()""").fetchall()
                observations["polls"] += 1
                for query, category, event in rows:
                    observations["active_backend_samples"] += 1
                    observations["waits"][(category or "Executing/runnable") + "/" + (event or "none")] += 1
                    observations["queries"][query_kind(query)] += 1
                    if category == "Lock":
                        observations["lock_queries"][query_kind(query)] += 1
                observations["poll_ms"] += (time.monotonic() - started) * 1000
                stop.wait(0.02)
    except Exception as error:
        observations["error"] = repr(error)


original_load = bench.load
original_start = bench.Stack.start


async def profiled_start(stack):
    await original_start(stack)
    stack.metadata["cpu_profiler"] = dict(
        version=bench.command("perf", "--version"), event="cpu-clock", frequency_hz=199,
        call_graph="dwarf,8192", database_poll_ms=20,
        wrapper_sha256=hashlib.file_digest(Path(__file__).open("rb"), "sha256").hexdigest(),
    )


async def profiled_load(stack, cell, duration):
    if cell["quota"] == "direct" or duration != stack.args.duration:
        return await original_load(stack, cell, duration)
    label = "-".join((cell["quota"], cell["sampling"], str(cell["concurrency"]),
                      "sse" if cell["stream"] else "json", str(cell["repeat"])))
    profile = stack.output / (label + ".perf.data")
    # This is the container created by this Stack, never a preexisting database.
    processes = bench.command("docker", "top", stack.container, "-eo", "pid,comm").splitlines()[1:]
    pg_pids = [line.split()[0] for line in processes if line.split()[1] == "postgres"]
    pids = [str(stack.router.pid), *pg_pids]
    observations = dict(polls=0, active_backend_samples=0, poll_ms=0,
                        waits=Counter(), queries=Counter(), lock_queries=Counter())
    stop = threading.Event()
    observer = threading.Thread(target=observe_database,
                                args=(stack.env["ROUTER_DATABASE_URL"], stop, observations), daemon=True)
    with (stack.output / (label + ".perf.log")).open("w") as log:
        recorder = subprocess.Popen(["sudo", "-n", "perf", "record", "--namespaces",
                                     "-e", "cpu-clock", "-F", "199",
                                     "--call-graph", "dwarf,8192", "-p", ",".join(pids),
                                     "-o", str(profile)], stdout=log, stderr=log,
                                    start_new_session=True)
        try:
            await asyncio.sleep(0.3)
            if recorder.poll() is not None:
                raise RuntimeError("perf failed to attach; see " + str(log.name))
            observer.start()
            result = await original_load(stack, cell, duration)
        finally:
            stop.set()
            if observer.ident is not None:
                await asyncio.to_thread(observer.join, 5)
            if recorder.poll() is None:
                os.killpg(recorder.pid, signal.SIGINT)
            try:
                await asyncio.wait_for(asyncio.to_thread(recorder.wait), 15)
            except asyncio.TimeoutError:
                os.killpg(recorder.pid, signal.SIGKILL)
                await asyncio.to_thread(recorder.wait)
    # sudo may propagate the requested SIGINT even after perf flushes normally.
    captured = "Captured and wrote" in Path(log.name).read_text() and profile.stat().st_size > 1024
    if recorder.returncode not in (0, 130, -signal.SIGINT) or not captured or observations.get("error") or observer.is_alive():
        raise RuntimeError("Incomplete diagnostic capture: " + repr(observations))
    observations.update(cell=cell, router_pid=stack.router.pid,
                        postgres_pids=pg_pids, perf_exit_code=recorder.returncode)
    (stack.output / (label + ".waits.json")).write_text(json.dumps(observations, indent=2) + "\n")
    # Resolve symbols while container processes and their mount namespaces exist.
    for name, selected_pids in (("router", str(stack.router.pid)),
                                ("postgres", ",".join(pg_pids))):
        with (stack.output / (label + "." + name + ".cpu.txt")).open("w") as report:
            subprocess.run(["sudo", "-n", "perf", "report", "--stdio", "--ns",
                            "--no-children", "-g", "none", "--percentage", "relative",
                            "--show-nr-samples", "--percent-limit", "0.1",
                            "--sort", "dso,symbol", "--pid", selected_pids, "-i", str(profile)],
                           stdout=report, stderr=subprocess.DEVNULL, check=True)
        with (stack.output / (label + "." + name + ".stacks.txt")).open("w") as report:
            subprocess.run(["sudo", "-n", "perf", "script", "--no-inline", "--pid", selected_pids,
                            "--max-stack", "128", "-i", str(profile)],
                           stdout=report, stderr=subprocess.DEVNULL, check=True)
    return result


if __name__ == "__main__":
    args = bench.parse_args()
    if args.duration == args.warmup or args.control_duration == args.duration:
        raise SystemExit("Use distinct measured, warmup and direct-control durations")
    bench.load = profiled_load
    bench.Stack.start = profiled_start
    with tempfile.TemporaryDirectory(prefix="quota-profile-proto-") as generated:
        include = str(Path(grpc_tools.__file__).parent / "_proto")
        bench.command(sys.executable, "-m", "grpc_tools.protoc", "-I" + str(bench.ROOT / "proto"),
                      "-I" + include, "--python_out=" + generated,
                      *(str(path) for path in sorted((bench.ROOT / "proto/chalk/router/v1").glob("*.proto"))))
        sys.path.insert(0, generated)
        signal.signal(signal.SIGTERM, lambda signum, frame: (_ for _ in ()).throw(KeyboardInterrupt()))
        asyncio.run(bench.run(args))
