# Router migrations

[Documentation index](../docs/README.md) · [Development](../docs/development.md)

The application **does not create tables at boot** — migrations must run first
when using Postgres.

| Source | Target DB (env) | When |
|---|---|---|
| `migrations/default` | `ROUTER_DATABASE_URL` | always |

`migrations/default` carries the core router schema, including API keys,
provider connections, usage, budgets, rate limits, and mutable quota state. The
plain-Postgres usage ledger and quota backend use this database and are the
deployed defaults (`ROUTER_USAGE_BACKEND=postgres` and
`ROUTER_QUOTA_BACKEND=postgres`; see
[chart environment helpers](../charts/chalk-router/templates/_helpers.tpl)).

Migration filenames should be concise; the SQL and its pull request are the
source of truth for a migration's rationale. Document only cross-migration
operational constraints here, such as a required backfill, a non-transactional
operation, or a compatibility rollout.

New router versions always write a connection ID. Events flushed by an old
replica during rollout may remain `NULL`.

Budget and rate-limit definitions always follow the core store. Their shared
mutable quota state is selected independently (`disabled`, `memory`, or
`postgres`); `redis` is reserved for a future implementation. There is no
separate quota migration source.

Migrations are applied **once per database**. The tables are multi-tenant:
tenant data is scoped by `environment_id`. Environment mode serves one configured
environment; cluster mode selects the environment per request. Both use the
same schema, so there is no per-environment migration.

## Secrets at rest

`chalk_router.provider_connections` stores each provider API key **encrypted**
(AES-256-GCM) in a single versioned `api_key_envelope` column —
`[version:1B][nonce:12B][ciphertext + tag]`. The router therefore
requires **`ROUTER_SECRETS_KEK`** (base64-encoded 32 bytes;
`openssl rand -base64 32`) whenever `ROUTER_DATABASE_URL` is set — it is loaded
alongside `ROUTER_KEY_PEPPER` via the same secret channel and the server
fail-fasts at boot without it. Keys cannot be hashed: they are replayed verbatim
to upstream providers, so they must be recoverable.

## Apply

For development, use a **local, disposable database** and the guarded recipe:

```sh
ROUTER_LOCAL_DATABASE_URL='postgresql://chalk:chalk-dev@127.0.0.1:5432/chalk' just migrate
```

`just migrate` loads the serving environment through `just run` and checks the
final database URL. Never bypass this check with direct Cargo/binary commands,
deployed credentials, or a localhost tunnel to a shared database. Running a local
process or changing `CHALK_ENVIRONMENT_ID` does not isolate schema changes.
Shared migrations belong to the approved deployment workflow.

The deployment Job uses the router-owned migration command below. It configures
SQLx 0.9 to use the qualified `chalk_router._sqlx_migrations` history table and
replaces SQLx's session advisory lock with transaction-pool-safe coordination.

```sh
# Deployment Job entrypoint; configuration comes from the approved deployment.
openai-server migrate
```

`ROUTER_DATABASE_URL` is required. In environment mode, `CHALK_ENVIRONMENT_ID`
is also required as a configuration tripwire for the Deployment that follows.
In cluster mode it is optional; the Job does not access the metadata API.
Migrations never read the environment ID and may not depend on a session GUC.

SQLx owns migration ordering, checksum validation, transactional execution,
dirty-state detection, and the qualified history table. The qualified history
table is created through SQLx's own `ensure_migrations_table`, so its
definition cannot drift from the SQLx version in use. All migration DDL must
remain `chalk_router.`-qualified; neither the runner nor the application relies
on session `search_path`. The command coordinates concurrent new runners with a
renewable lease and disables SQLx's session-scoped PostgreSQL advisory lock.
The lease serializes healthy runners, but it does not cancel or fence an
in-flight `MIGRATOR.run` after expiry. If a handoff overlaps a transactional
migration, SQLx's atomic DDL-and-history transaction and the history table's
version primary key prevent a double apply. Existing no-transaction migrations
are restricted to one declared concurrent index and have exact-definition crash
recovery metadata.

On every run, the compatibility preflight recognizes exact router history rows
accidentally written to `public._sqlx_migrations` by the previous
search-path-based runner and copies missing rows into the qualified history
table. Rows are adopted only when their version and checksum match the embedded
router manifest; unrelated public SQLx history is ignored. Each adopted row is
logged at `info`. The preflight is a one-shot rollout shim; delete it once
production has completed one migrate run with that log silent.

### Compatibility rollout

The qualified table name is deliberately unchanged from the table selected by
the old runner's `SET search_path = chalk_router`, so an environment already
migrated by that runner requires no history move. The renewable lease
serializes healthy instances of the new runner, but cannot stop an in-flight
migration after expiry or coordinate with the old runner's session advisory
lock. Mixed-version migration Jobs must therefore remain externally serialized:
delete and wait for the fixed-name
`chalk-router-migrate` Job before creating its replacement.

## Schema snapshots

The root `schema.sql` is **generated, not edited**. It is a canonical snapshot
of what the migration source produces when applied to a fresh, digest-pinned
database container, with migration history and lease bookkeeping excluded:

| Snapshot | Source | Built in |
|---|---|---|
| `schema.sql` | `migrations/default` | PostgreSQL 16 |

After adding a migration, regenerate and commit the migration and the updated
snapshot together:

```sh
just schema-write
just schema-check            # reproduce and fail on drift
```

`--check` runs in the public GitHub CI workflow with disposable PostgreSQL.
Snapshots make schema changes reviewable as a diff.

Notes:

- `scripts/schema.sh` needs Docker. Its pinned PostgreSQL image is pulled
  anonymously from Docker Hub; no private registry credentials are needed.
- `schema.sql` is produced with `openai-server migrate`, the same command the
  k8s Job runs, so the check also exercises qualified SQLx history, renewable
  lease coordination, and the invalid-index guard in `src/migrate.rs`.
- Postgres is the implemented durable usage backend. The reserved `clickhouse`
  configuration does not provide an active usage backend or a migration source.

## Recovering a failed `CREATE INDEX CONCURRENTLY`

Index migrations open with `-- no-transaction` so they can build concurrently
without locking writes. The tradeoff is that a failed build is *not* rolled
back: Postgres keeps the index and marks it invalid, where the planner ignores
it but every write still maintains it. The DDL and its successful history write
cannot be atomic, so a crash can also leave the name taken while the migration
is still pending.

Before SQLx runs, the router checks pending non-transactional migrations against
explicit recovery metadata for every concurrent index. If the
expected valid index exists, the runner records the matching embedded SQLx
history without replaying DDL. Invalid or unexpectedly defined indexes fail
with the offending name. A manifest unit test requires every
`-- no-transaction` migration to have recovery metadata; add metadata alongside
any future non-transactional migration.

For an approved recovery operation on the affected database (or a disposable
local reproduction), repair the reported index before rerunning migrations:

```sh
psql "$ROUTER_DATABASE_URL" -c 'REINDEX INDEX CONCURRENTLY chalk_router.<index_name>'
# or, if the index is no longer wanted:
psql "$ROUTER_DATABASE_URL" -c 'DROP INDEX CONCURRENTLY chalk_router.<index_name>'
```

`REINDEX` can itself fail (it is the same concurrent build), leaving a
`..._ccnew` invalid index behind; drop that and retry. Do not add
`IF NOT EXISTS` to these migrations to work around a collision — it matches on
name only, so it would skip the invalid index and record the migration as
applied.

All router tables live in the shared `chalk_router` Postgres schema.

## Tenancy

`environment_id` is a security boundary: tenant stores filter queries by their
runtime's environment. Environment mode uses `CHALK_ENVIRONMENT_ID`; in
[cluster mode](../docs/cluster-router.md), authentication selects the runtime.
The shared router-key lookup resolves ownership before selecting that runtime.
Tenant isolation is enforced by application stores, not Postgres Row-Level
Security. RLS remains a possible defense against a missing environment filter,
but any future design must respect transaction-pool-safe scoping; session state
cannot be assumed to stay attached to one client.
