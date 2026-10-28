"""Aggregate repetitions without pooling percentiles; label unhealthy cells."""

import argparse
from collections import defaultdict
import json
from pathlib import Path
from statistics import median


def aggregate(directory):
    groups = defaultdict(list)
    for line in (directory / "results.jsonl").read_text().splitlines():
        row = json.loads(line)
        groups[(row["quota"], row["sampling"], row["concurrency"], row["stream"])].append(row)
    result = []
    for (quota, sampling, concurrency, stream), rows in sorted(groups.items()):
        item = dict(quota=quota, sampling=sampling, concurrency=concurrency, stream=stream,
                    repetitions=len(rows), successful=sum(r["successful"] for r in rows),
                    healthy=all(r.get("healthy", True) for r in rows),
                    health_instrumented=all("healthy" in r for r in rows),
                    deadlocks=sum(r.get("deadlocks", 0) for r in rows))
        for key in ("rps", "p50_ms", "p95_ms", "p99_ms", "ttft_p50_ms", "ttft_p95_ms"):
            values = [r[key] for r in rows if r[key] is not None]
            item[key] = median(values) if values else None
            if key == "rps":
                item["rps_min"], item["rps_max"] = min(values), max(values)
        sql_counts = [sum(
            q["calls"] for q in r["sql"] if q.get("toplevel", True)
            and q["query"] != "SELECT count(*) FROM chalk_router.usage_events"
            and "FROM pg_stat_wal" not in q["query"]
        ) / r["successful"] for r in rows if r["successful"]]
        item["sql_calls_per_request"] = median(sql_counts) if sql_counts else None
        for process in ("router", "mock", "driver"):
            values = [r["cpu_pct"][process] for r in rows if r["cpu_pct"][process] is not None]
            item[process + "_cpu_pct"] = median(values) if values else None
        for key in ("frontend_spans", "root_spans", "llm_spans", "content_spans", "bytes"):
            values = [r["exports"][key] / r["successful"] for r in rows
                      if r["successful"] and key in r["exports"]]
            item["export_" + key + "_per_request"] = median(values) if values else None
        result.append(item)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    result = aggregate(args.directory)
    (args.directory / "summary.json").write_text(json.dumps(result, indent=2) + "\n")
    lines = ["# Median results across repetitions", "",
             "Percentiles are medians of per-repetition percentiles. Throughput ranges show repeat variability.", "",
             "| Quota | Sampling | Concurrency | Delivery | Reps | Requests | req/s median [range] | p50 ms | p95 ms | TTFT p50 ms | Healthy |",
             "| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |"]
    for r in result:
        health = str(r["healthy"]) if r["health_instrumented"] else "uninstrumented"
        if r["quota"] == "direct" and r["healthy"]:
            health = "control"
        ttft = f"{r['ttft_p50_ms']:.2f}" if r["ttft_p50_ms"] is not None else "—"
        p50 = f"{r['p50_ms']:.2f}" if r["p50_ms"] is not None else "—"
        p95 = f"{r['p95_ms']:.2f}" if r["p95_ms"] is not None else "—"
        lines.append(f"| {r['quota']} | {r['sampling']} | {r['concurrency']} | "
                     f"{'SSE' if r['stream'] else 'JSON'} | {r['repetitions']} | {r['successful']} | "
                     f"{r['rps']:.1f} [{r['rps_min']:.1f}, {r['rps_max']:.1f}] | "
                     f"{p50} | {p95} | {ttft} | {health} |")
    (args.directory / "summary.md").write_text("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
