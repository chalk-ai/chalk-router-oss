# chalk-router

LLM proxy (Rust/Axum): OpenAI- and Anthropic-compatible HTTP APIs, gRPC admin,
Next.js UI in `ui/`. Crate and binary are `openai-server`, not `chalk-router`.
Architecture: `rfp/0001-router-motivation-and-architecture.md`.

## Commands

Prefer `just` over raw cargo/npm (`just --list`). Common gotchas:

- `just run` — authenticated `127.0.0.1:4001`; uses explicit environment or local `.env`, never decrypts the serving bundle
- `just ci` is the gate: `cargo fmt -p openai-server -- --check`, then exactly
  `cargo clippy --locked -p openai-server -- -D warnings` (no `--all-targets`;
  that variant reports lints CI does not enforce), then `cargo nextest run --profile ci`.
  `just test` is not CI. The `/router-gate` skill runs the same sequence.
- Postgres tests skip unless `TEST_DATABASE_URL` is set. Run them with
  `just test-db` against a throwaway Postgres 16 (never a shared database): the
  migration tests serialize on one advisory lock and deadlock under nextest's
  default parallelism, so the `postgres` test group in `.config/nextest.toml`
  is pinned to one thread. `migrate_bookkeeping_test` counts the files in
  `migrations/default/`; a new migration must update it in the same PR. CI
  runs this group and schema/upgrade checks in the public GitHub workflow.
  `just test-db-docker` boots a throwaway container locally and runs the group.
- Vertex mock tests require `openssl` on PATH to generate an ephemeral RSA key; no credential file or network access is needed for key generation.
- Paid live tests: `just test-live` with `ROUTER_LIVE_API_TESTS=1`.
- After `proto/` edits: `just gen-proto`. After migrations: `just schema-write`, then `just schema-check`.
- UI: `cd ui && npm test` (vitest), `npx tsc --noEmit -p .`, `npm run build`.
- UI styling follows the Chalk dashboard's design system (chalk-private
  `frontend`, dark theme only). `ui/src/app/globals.css` holds its tokens and
  recipes: use `bg-cds-*`/`text-cds-ink-*`/`border-cds-*` classes, or
  `var(--color-cds-*)` in inline styles, and `.btn`/`.field`/`.badge`/`.card`/
  `.table-cds`; not raw gray/slate stops or hex. `AppShell` is the rail and
  breadcrumb shell; a new top-level page belongs to one of its categories.

`just` is the local development interface. No company deployment pipeline or
secret bundle is included. Deployment and provider seeding use explicit operator
configuration; never apply changes to a shared cluster or database without authorization.
Without a database the router uses in-memory stores. Never commit a local `.env`.

## Development migrations

Run development migrations only against a local, disposable database. Use
`just migrate`; `just run migrate` also checks the final database URL after
loading secrets and applying overrides. Never bypass these guards with direct
binary/Cargo commands, deployed credentials, cluster commands, or a localhost
tunnel to a shared database to fix local setup. A local process or a different
`CHALK_ENVIRONMENT_ID` does not isolate schema changes in a shared database.
Shared database migrations belong to the approved deployment workflow;
permission to implement a schema change does not authorize applying it there.

## Layout

- `src/handlers/` — thin HTTP adapters for the proxy APIs; the surface × stage
  table at the top of `src/handlers/mod.rs` says which pipeline stage applies to
  which surface and why. Update it when that changes.
- `src/adapters/` — execute translated requests through backend interfaces; no routing, retries, authentication, or quotas.
- `src/translation/` — pure format conversion and event encoding; `anthropic_chat` and `responses_chat` convert Anthropic Messages and OpenAI Responses requests to Chat Completions and convert replies back.
- `src/backend/` — upstream clients + model routing
- `src/service/` — wire-agnostic admin logic behind the gRPC adapters; gRPC
  (`src/grpc/`) is the only admin surface.
- `src/grpc/` — tonic adapters only
- `src/telemetry/` — OTLP + Braintrust exporters
- `migrations/default/` — SQLx; the app does not create tables at boot
- `tests/integration/` — in-process suite (CI), one binary; shared fixtures live
  in `tests/integration/support/`. `tests/integration_test.rs` is paid live and off by default.
- `ui/src/gen/` — generated; never edit by hand
- `proto/chalk/auth/`, `proto/chalk/utils/` — byte-for-byte copies from chalk-private
  (`just sync-chalk-protos`); every `chalk.router.v1` RPC declares
  `option (chalk.auth.v1.permission)`, and `build.rs` fails the build without it.
- `changelogs/YYYY-MM-DD/<who>-<topic>.md` — one bullet per file describing what a
  user of the router or its admin UI notices. No file names, no implementation
  notes; those belong in the commit message.

Admin UI talks gRPC-Web via `ui/src/lib/grpc.ts` from server components, and
client components call the `/api/admin/*` route handlers (the BFF), not a new
REST client. BFF input validation lives in `ui/src/lib/bff.ts` and mirrors the
Rust service rules.

## Shared helpers — use these, don't hand-roll

- Request pipeline: `handlers/routing.rs::try_with_fallbacks_if` (fallback walk
  and one quota reservation for the whole request), `acquire_request_quota`,
  `run_passthrough` (resolve + capability check for non-chat surfaces);
  `handlers/usage.rs::with_quota_settlement` (usage write + quota settle).
  See the `/new-llm-surface` skill before adding or changing a handler.
- SSE: `handlers/sse.rs::{encode_frame, done_frame, SseDecoder}`. Error frames
  are built from `types::ErrorResponse::new`, never from hand-written JSON.
- Capability requirements: builders live in `src/capability_requirements.rs`.
- Upstream errors: `backend/provider.rs::upstream_error`; unsupported operations
  are `BackendError::api_status(400, ..)`. Base URLs: `provider_registry.rs::resolve_base_url`.
  Adapter construction: `backend/router.rs::build_provider_adapter` (traffic and probe).
  Route prefixes: `provider_registry.rs::strip_route_prefix` and `ROUTE_RESERVED_PREFIX`.
- Stores: every store has in-memory and Postgres implementations (and a cached
  wrapper where reads are hot); a change lands in all of them, and the dual-backend
  conformance tests are the proof. Revisioned singletons (fallback, judge) use
  `src/revisioned_policy.rs`. `sqlx::Error` → domain error goes through
  `db_error::classify`. Validation shared by the API-key and usage-pool services:
  `ratelimit.rs::{normalize_rate_limits, validate_rate_limits}`, `budget.rs::validate_usage_budgets`.
- Configuration: every environment variable is read in `config.rs::RouterConfig`
  (`env_nonempty` / `env_parse` / `env_flag`). The only exceptions are the
  `migrate` subcommand's check in `main.rs` and `OTEL_*` variables the SDK reads itself.
- Quota code constraints (from review, do not reopen): the reservation fold in
  `try_with_fallbacks_if` must not skip unknown-reservation routes; `finish_failed()`
  is not for partial streams; lease expiry and Drop cleanup in `quota.rs` stay;
  `set_key_daily_budget` and its CTE are not to be touched.

## Tracing

LLM spans follow OpenInference
(https://github.com/Arize-ai/openinference/blob/main/spec/semantic_conventions.md):
`openinference.span.kind=LLM`, model/system, input/output messages, token
counts, `llm.invocation_parameters`, `input`/`output` value+mime.

Stamp them through `src/handlers/trace_attributes.rs` and
`LlmSpanWriter`; handler code must not call `set_attribute` directly. The
writer owns content logging, attribute-count allocation, byte budgets, and
direction-specific truncation. Braintrust (`src/telemetry/braintrust.rs`)
forwards those attributes as-is — skipping them loses both destinations;
vendor-specific fixups belong in that exporter, not in handlers.

## Working with agents

- CLI preferences: search with `rg` (and `rg --files` to list files), not
  `grep -r` or `find`. Disk usage with `dust` (installed here) rather than
  `du -sh` on `target/`, which takes tens of seconds.
- Large files: `src/backend/router.rs`, `src/capabilities.rs`, `src/handlers/chat.rs`,
  `src/handlers/anthropic.rs`, `src/handlers/responses.rs`, `src/backend/anthropic.rs`
  are 2,000–3,700 lines. `rg -n` first, then read only the matched region;
  never read `prices/`, `tests/fixtures/`, `ui/src/gen/`, `ui/node_modules/`,
  `ui/package-lock.json`, or `Cargo.lock` into context.
- Parallel worktrees each need their own build directory: `just worktree <name> [base]`
  creates one under `.worktrees/` with `target/` cloned from this checkout, and
  `just seed-target` does the same inside a worktree another tool created. A
  shared target directory serializes builds behind cargo's lock and has produced
  phantom compile errors.
- The `/router-subagent-brief` skill emits the standard preamble for a subagent brief.
- Skills are committed once under `.agents/skills/` (the tool-neutral location
  Codex reads); `.claude/skills` and `.opencode/skills` are symlinks to it.
  `CLAUDE.md` only imports this file. The rest of `.claude/` and `.opencode/` is local.
