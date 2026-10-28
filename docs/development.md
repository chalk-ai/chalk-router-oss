# Development

[Documentation index](README.md) · [Repository overview](../README.md)

Read [AGENTS.md](../AGENTS.md) before changing the repository. Commands below run
from the repository root unless a different directory is shown. `just --list`
lists the recipes in [Justfile](../Justfile); the crate and binary are
`openai-server`.

## Local database and secrets

Follow [self-managed setup](self-managed-setup.md). Local startup requires an
explicit admin key and binds loopback; no encrypted bundles are loaded. Use only
an owned disposable PostgreSQL 16 database for migrations and tests. The local
migration guard rejects non-loopback URLs; do not bypass it using a tunnel to a
shared database. Preserve the pepper and encryption key for persisted data.
`just setup` installs UI dependencies; `just seed --file <path>` consumes an
explicit synthetic/operator-owned file and database configuration.

## Testing

| Check | Command | Requirements / effects |
| --- | --- | --- |
| Rust CI gate | `just ci` | Format check, Clippy, then nextest CI profile |
| Rust unit and in-process tests | `just test` | Does not replace the CI gate; paid provider suite is opt-in |
| Database tests | `just test-db-docker` | Owns and removes a throwaway Postgres container; Docker and registry access |
| Existing disposable database | `TEST_DATABASE_URL=<local-disposable-url> just test-db` | Writes test data; nextest serializes the `postgres` group |
| UI | See commands below | Installs pinned packages, typechecks, runs Vitest, and builds |
| Mock upstream contract | `just mock-openai-test` | Python 3.13 via `uv`; synthetic ASGI tests |
| JSON allocation benchmark | `just schema-json-bench` | Synthetic local measurements; no provider or database |
| Load/tenant benchmarks | [Benchmark guide](../test_services/router_bench/README.md) | Owns isolated local Postgres, mock, and router processes |
| SDK smoke test | [Client guide](../frontend_test/README.md) | Running router and configured providers; may incur provider charges |
| Paid live suite | `ROUTER_LIVE_API_TESTS=1 just test-live` | Explicit live credentials; makes real provider calls |

The Rust gate matches [.github/workflows/ci.yml](../.github/workflows/ci.yml):

```sh
cargo fmt -p openai-server -- --check
cargo clippy --locked -p openai-server -- -D warnings
cargo nextest run --profile ci
```

Do not add `--all-targets` to the Clippy gate: it enables lints CI does not enforce.
Postgres tests skip when `TEST_DATABASE_URL` is unset. CI supplies a throwaway
database; a local nextest run without one does not exercise that coverage.
The [nextest configuration](../.config/nextest.toml) serializes database tests
to avoid lock conflicts. Never point these tests at a shared database.

UI validation (Node.js 22, as used by [the UI image](../ui/Dockerfile)):

```sh
npm --prefix ui ci
cd ui
npx tsc --noEmit -p .
npm test
npm run build
```

The [legacy OpenCode/Anthropic script](../integration_tests/test_opencode_anthropic.sh)
predates stored provider connections: it loads `.env`, starts its own router,
and forwards `ANTHROPIC_API_KEY` as a client credential. It is not a current
authentication smoke test and needs adaptation before use. Use the
[CLI guide](router-cli.md) for client configuration and diagnostics.

## Code generation and schema changes

- After editing router protos, run `just gen-proto`; never hand-edit
  `ui/src/gen/`. `proto/chalk/auth/` and `proto/chalk/utils/` are copied from
  chalk-private with `just sync-chalk-protos`.
- After adding a migration, run `just schema-write` and `just schema-check`,
  commit the generated snapshot, and update the migration count in
  `migrate_bookkeeping_test`. See [schema snapshots](../migrations/README.md#schema-snapshots)
  for the isolated Docker workflow and registry requirements.
- Add user-visible change bullets under [changelogs](../changelogs/README.md);
  keep implementation detail in the commit or PR description.

## Architecture

HTTP inference handlers and gRPC/gRPC-Web admin services share the listener on
port 4001. Administration is gRPC-only; `/v1/admin/*` HTTP routes were removed.
The Next.js server uses [grpc.ts](../ui/src/lib/grpc.ts) to call the admin API.
Browser components use the UI's `/api/admin/*` handlers, whose input validation
is shared through [bff.ts](../ui/src/lib/bff.ts). These UI routes do not restore
a backend REST admin API. See the [authentication transition](authentication-transition.md)
for per-user session and backend authorization requirements.

| Location | Responsibility |
| --- | --- |
| [src/main.rs](../src/main.rs) | Startup and route registration |
| [src/handlers/](../src/handlers/) | HTTP inference adapters; `mod.rs` maps pipeline stages by surface |
| [src/grpc/](../src/grpc/) | Thin tonic admin adapters |
| [src/service/](../src/service/) | Shared admin business logic |
| [src/backend/](../src/backend/) | Upstream clients and model routing |
| [src/adapters/](../src/adapters/) · [src/translation/](../src/translation/) | Backend execution and pure protocol conversion |
| [src/telemetry/](../src/telemetry/) | Exporters and bounded span handling |
| [proto/chalk/router/v1/](../proto/chalk/router/v1/) | Admin API contracts |
| [migrations/default/](../migrations/default/) | Core Postgres schema |
| [tests/integration/](../tests/integration/) | In-process integration suite and shared fixtures |
| [ui/](../ui/) | Next.js admin UI and playground |

Both in-memory and Postgres stores implement the domain contracts. Shared
budget/rate-limit shape validation belongs in the policy modules so API-key and
usage-pool services agree. See [AGENTS.md](../AGENTS.md#shared-helpers--use-these-dont-hand-roll)
for required helpers and [the RFP index](../rfp/README.md) for design rationale.
