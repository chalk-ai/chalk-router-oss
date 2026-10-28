"""Compare matching load runs and audit their provenance, health and cleanup."""

import argparse
import json
from pathlib import Path

from summarize import aggregate


def key(row):
    return row["quota"], row["sampling"], row["concurrency"], row["stream"]


def audit(directory):
    metadata = json.loads((directory / "metadata.json").read_text())
    cleanup = json.loads((directory / "cleanup.json").read_text())
    rows = [json.loads(line) for line in (directory / "results.jsonl").read_text().splitlines()]
    router = [row for row in rows if row["quota"] != "direct"]
    assert router and all(row["healthy"] for row in router), directory
    assert all(row["requests"] == row["successful"] for row in rows), directory
    if metadata["arguments"]["usage_backend"] == "postgres":
        assert all(row["usage_rows"] == row["successful"] for row in router), directory
    assert cleanup["verified"] and cleanup["container_removed"], directory
    assert not any(cleanup["after"].values()), directory
    return metadata, dict(
        router_measurements=len(router),
        direct_controls=len(rows) - len(router),
        unique_configurations=len({key(row) for row in router}),
        successful_proxied_requests=sum(row["successful"] for row in router),
        deadlocks=sum(row["deadlocks"] for row in router),
        healthy=True, exact_usage=metadata["arguments"]["usage_backend"] == "postgres",
        cleanup_verified=True, container_removed=True,
        binary_source_revision=metadata["binary_source_revision"],
        binary_sha256=metadata["binary_sha256"],
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    before_metadata, before_audit = audit(args.before)
    after_metadata, after_audit = audit(args.after)
    for field in ("driver_sha256", "lock_sha256", "mock_sha256", "postgres_image_id"):
        assert before_metadata[field] == after_metadata[field], field
    ignored = {"binary", "binary_revision", "output", "expect_outcome_retention"}
    assert {k: v for k, v in before_metadata["arguments"].items() if k not in ignored} == {
        k: v for k, v in after_metadata["arguments"].items() if k not in ignored
    }, "scenario settings differ"
    before = {key(row): row for row in aggregate(args.before)}
    after = {key(row): row for row in aggregate(args.after)}
    assert before.keys() == after.keys(), "matrix differs"
    comparisons = []
    for cell in sorted(before):
        first, last = before[cell], after[cell]
        comparisons.append(dict(
            quota=cell[0], sampling=cell[1], concurrency=cell[2], stream=cell[3],
            before=first, after=last,
            p50_change_pct=(last["p50_ms"] / first["p50_ms"] - 1) * 100,
            rps_change_pct=(last["rps"] / first["rps"] - 1) * 100,
        ))
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output / "comparison.json").write_text(json.dumps(comparisons, indent=2) + "\n")
    (args.output / "audit.json").write_text(json.dumps(dict(before=before_audit, after=after_audit), indent=2) + "\n")
    lines = ["# Matching-source before/after comparison", "",
             "Medians across repetitions; SQL counts include bounded refreshes. Closed-loop localhost measurements.", "",
             "| Quota | Sampling | c | Delivery | p50 before → after ms | p50 change | req/s before → after | req/s change | SQL/request before → after |",
             "| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: |"]
    for row in comparisons:
        first, last = row["before"], row["after"]
        lines.append(f"| {row['quota']} | {row['sampling']} | {row['concurrency']} | "
                     f"{'SSE' if row['stream'] else 'JSON'} | {first['p50_ms']:.2f} → {last['p50_ms']:.2f} | "
                     f"{row['p50_change_pct']:+.1f}% | {first['rps']:.1f} → {last['rps']:.1f} | "
                     f"{row['rps_change_pct']:+.1f}% | {first['sql_calls_per_request']:.3f} → {last['sql_calls_per_request']:.3f} |")
    (args.output / "comparison.md").write_text("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
