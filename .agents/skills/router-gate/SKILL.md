---
name: router-gate
description: Run chalk-router's exact CI gate (fmt-check, clippy -D warnings, nextest) and, on request, the Postgres-gated test group against a disposable Postgres 16 container. Use before reporting any Rust change as done, and whenever a "clippy failure" or "DB test failure" needs confirming.
---

# router-gate

Runs what `.github/workflows/ci.yml` runs, with the same flags. CI
provisions disposable Postgres and runs the `postgres` group with the rest of
nextest; local runs need `TEST_DATABASE_URL` or the separate database gate below.
Report each step's last lines verbatim; never summarise a failure as
"pre-existing" without the evidence below.

## 1. The CI gate (always)

Run from the crate root (the worktree you are editing), in this order, each as
its own command with a 600000 ms timeout:

```
cargo fmt -p openai-server -- --check
cargo clippy --locked -p openai-server -- -D warnings
cargo nextest run -p openai-server --profile ci
```

Rules:
- The clippy command is exactly the one above. `--all-targets` and `--tests`
  enable lints CI does not enforce and produce failures nobody has to fix.
- If `fmt --check` fails, run `cargo fmt -p openai-server` and re-check.
- In a worktree, run `just seed-target` once first so cargo uses this worktree's
  own `target/` (cloned from the main checkout). Never point several worktrees at
  one target directory: cargo serializes them behind its build lock and the
  shared artifacts have produced phantom compile errors in files nobody touched.
- A test that fails once and passes on rerun under load
  (`catalog_walk_has_one_overall_deadline` is the known one) is a timing flake;
  report it as such with both outcomes.

## 2. The Postgres-gated group (when the change touches stores, migrations, quota, budgets, or usage)

If section 1's nextest run already used a disposable `TEST_DATABASE_URL`, it
covered this group too; do not run it a second time against the same database.

Prefer the Justfile recipe:

```
just test-db
```

It expects `TEST_DATABASE_URL`. If none is set, provision a throwaway database
(never point it at a shared or development database; the tests create and drop
schema):

```
docker run -d --name chalk-router-gate-pg \
  -e POSTGRES_USER=test -e POSTGRES_PASSWORD=test -e POSTGRES_DB=chalk_router_test \
  -p 5499:5432 postgres:16
until PGPASSWORD=test pg_isready -h 127.0.0.1 -p 5499 -U test >/dev/null 2>&1; do sleep 1; done
TEST_DATABASE_URL=postgres://test:test@127.0.0.1:5499/chalk_router_test just test-db
docker rm -f chalk-router-gate-pg
```

If `just test-db` is missing on the branch, the equivalent is:

```
TEST_DATABASE_URL=... cargo nextest run -p openai-server -j1 --no-fail-fast \
  -E 'test(/migrat/) | test(/postgres/) | test(/usage_snapshot/) | test(/conformance/) | test(/rpc_performance/)'
```

Rules:
- `-j1` is mandatory: the migration tests serialize on one Postgres advisory
  lock and deadlock under nextest's default parallelism (the runner blocks a
  `CREATE INDEX CONCURRENTLY` that blocks the runner; Postgres does not detect it).
- `--no-fail-fast`, so one failure does not hide the rest.
- `migrate_bookkeeping_test::router_sqlx_migrations_are_backward_compatible_and_isolated`
  asserts the number of files in `migrations/default/`. If it fails with
  `left: N+1, right: N`, a migration landed without updating the test; fix the
  test in the same change, do not skip it.
- Reuse a database only within one run. Stale rows from an earlier run trip the
  unique constraints in `ratelimit_conformance_test`; drop and recreate instead.

## 3. UI (when `ui/` changed)

```
cd ui && npx tsc --noEmit -p . && npm test && npm run build
```

## Reporting

For each command: the command, exit status, and its last three lines. For any
failure: whether the same command fails on the base branch (check it, do not
guess), the failing test or lint name, and the file:line it points at.
