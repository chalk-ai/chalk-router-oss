package := "openai-server"
bin := "openai-server"
rust_log := "info,openai_server=debug"
local_database_url := "postgresql://router:router-local@127.0.0.1:5432/router"

default:
    @just --list

# Format Rust sources
fmt:
    cargo fmt -p {{package}}

# Fail if Rust sources are unformatted
fmt-check:
    cargo fmt -p {{package}} -- --check

# Typecheck
check:
    cargo check --locked -p {{package}}

# Build the router binary
build:
    cargo build --locked -p {{package}} --bin {{bin}}

# Optimized binary for the isolated Chat Completions load benchmark.
bench-chat-build:
    cargo build --locked --release -p {{package}} --bin {{bin}}

# Owns a fresh localhost Postgres/mock/router stack and verifies DB cleanup.
bench-chat *args:
    uv run --project test_services/router_bench --locked python test_services/router_bench/bench.py {{args}}

# Compare router binaries on simple, hot-key and multi-tenant quota load (owned stacks).
bench-quota-tenants *args:
    uv run --project test_services/router_bench --locked python test_services/router_bench/quota_tenants.py {{args}}

# Sample the owned router/Postgres CPU stacks and active database wait events.
bench-chat-profile *args:
    uv run --project test_services/router_bench --locked python test_services/router_bench/quota_profile.py {{args}}

# Count ownership allocations in the native Chat JSON response conversion.
bench-chat-copies:
    cargo run --locked --release -p {{package}} --example chat_response_copy_bench

# Unit + in-process tests (skips the paid live API suite)
test *args:
    cargo test --locked -p {{package}} {{args}}

# Same suite CI runs
test-ci:
    cargo nextest run --profile ci

# Postgres-gated integration tests against a throwaway database (never a
# shared one). Requires TEST_DATABASE_URL. The `postgres` test group in
# .config/nextest.toml pins these to one thread; `group()` selects exactly
# that group so the rest of the suite (already covered by `just test`/`test-ci`)
# doesn't get re-run here.
test-db *args:
    cargo nextest run -p {{package}} -E 'group(postgres)' {{args}}

# Local equivalent of the Postgres coverage in CI's nextest step: boots a
# throwaway PostgreSQL container (never a shared database), applies migrations,
# runs only the `postgres` group, and tears the container down again.
test-db-docker *args:
    #!/usr/bin/env bash
    set -euo pipefail
    url="$(scripts/test-postgres.sh start)"
    trap 'scripts/test-postgres.sh stop' EXIT
    ROUTER_DATABASE_URL="$url" CHALK_ENVIRONMENT_ID=test-db \
        cargo run --locked --quiet --bin openai-server -- migrate
    TEST_DATABASE_URL="$url" just test-db {{args}}

# Synthetic retained-JSON allocation and timing measurements (no network).
schema-json-bench:
    cargo run --locked -p {{package}} --example schema_json_bench

# Paid OpenAI / Anthropic / Voyage round-trips (needs keys + ROUTER_LIVE_API_TESTS=1)
test-live *args:
    cargo test --locked -p {{package}} --test integration_test {{args}} -- --nocapture

# Clippy with warnings denied
clippy:
    cargo clippy --locked -p {{package}} -- -D warnings

# Same gate as .github/workflows/ci.yml
ci: fmt-check clippy test-ci

# Run the synthetic OpenAI/Anthropic upstream used for local provider and
# router development. The server stays in the foreground. Its base URL is
# http://127.0.0.1:<port>/v1.
mock-openai port="18000":
    #!/usr/bin/env bash
    set -euo pipefail
    mock_openai_port="{{port}}"
    if ! [[ "$mock_openai_port" =~ ^[0-9]+$ ]] || (( mock_openai_port < 1 || mock_openai_port > 65535 )); then
        echo "port must be an integer between 1 and 65535" >&2
        exit 2
    fi
    mock_openai_dir="test_services/mock_openai"
    uv sync --project "$mock_openai_dir" --locked --no-install-project --python 3.13
    echo "Mock OpenAI/Anthropic provider base URL: http://127.0.0.1:$mock_openai_port/v1"
    exec "$mock_openai_dir/.venv/bin/python" -m uvicorn app:app \
        --app-dir "$mock_openai_dir" \
        --host 127.0.0.1 \
        --port "$mock_openai_port" \
        --workers 1

# Alias for mock-openai: both commands start the same app.py server,
# which serves both OpenAI and Anthropic endpoints.
mock-anthropic port="18000": (mock-openai port)

# Exercise the mock's ASGI concurrency and cancellation lifecycle.
mock-openai-test:
    uv sync --project test_services/mock_openai --locked --no-install-project --python 3.13
    test_services/mock_openai/.venv/bin/python -m unittest discover -s test_services/mock_openai -p 'test_*.py' -v

# Authenticated, loopback-only local router. No encrypted serving bundle is loaded.
# Export ROUTER_ADMIN_KEY for management and inference; see README for setup.
run *args:
    #!/usr/bin/env bash
    set -euo pipefail
    export RUST_LOG="${RUST_LOG:-{{rust_log}}}"
    export CHALK_ENVIRONMENT_ID="${CHALK_ENVIRONMENT_ID:-develop}"
    set -- {{args}}
    if [[ "${1:-}" == "migrate" ]]; then
        export ROUTER_DATABASE_URL="${ROUTER_DATABASE_URL:-${ROUTER_LOCAL_DATABASE_URL:-{{local_database_url}}}}"
        ROUTER_LOCAL_DATABASE_URL="$ROUTER_DATABASE_URL" just --justfile "{{justfile_directory()}}/Justfile" check-local-db
    fi
    exec cargo run --locked -p {{package}} --bin {{bin}} -- "$@"

# Apply chalk_router migrations to a local database only.
migrate binary="": check-local-db
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -n "{{binary}}" ]]; then
        # Benchmarks use a frozen binary and an owned database without loading
        # serving secrets. Check the final URL, just as `run migrate` does.
        ROUTER_LOCAL_DATABASE_URL="${ROUTER_DATABASE_URL:?set ROUTER_DATABASE_URL}" just --justfile "{{justfile_directory()}}/Justfile" check-local-db
        exec "{{binary}}" migrate
    fi
    just run migrate

# Seed an explicit local JSON file; provider credentials come from your environment.
seed *args:
    just run seed-connections {{args}}

# Install UI dependencies. The authenticated in-memory router needs no database.
setup:
    npm --prefix ui ci

# Early check for local maintenance recipes. `run migrate` repeats the check
# against the final ROUTER_DATABASE_URL after explicit overrides are resolved.
check-local-db:
    #!/usr/bin/env python3
    import os
    import sys
    from urllib.parse import parse_qsl, urlsplit

    database_url = os.environ.get("ROUTER_LOCAL_DATABASE_URL") or "{{local_database_url}}"
    try:
        if any(char.isspace() for char in database_url):
            raise ValueError("malformed URL")
        parsed = urlsplit(database_url)
        if parsed.scheme not in {"postgres", "postgresql"} or parsed.fragment:
            raise ValueError("invalid PostgreSQL URL")
        if parsed.hostname not in {"localhost", "127.0.0.1", "::1"}:
            raise ValueError("non-local host")
        # Accessing port also rejects malformed authorities and multi-host URLs.
        if parsed.port == 0:
            raise ValueError("invalid port")
        if any(key.lower() in {"host", "hostaddr", "service", "servicefile"}
               for key, _ in parse_qsl(parsed.query)):
            raise ValueError("connection destination override")
    except ValueError:
        print("Refusing to run: use a valid PostgreSQL URL with host localhost, "
              "127.0.0.1, or [::1], without host/service overrides. Shared database "
              "migrations belong to the approved deployment workflow.", file=sys.stderr)
        sys.exit(1)

# Local admin UI; uses only explicitly supplied environment credentials.
ui:
    #!/usr/bin/env bash
    set -euo pipefail
    export CHALK_ROUTER_URL="${CHALK_ROUTER_URL:-http://127.0.0.1:4001}"
    exec npm --prefix ui run dev -- --hostname 127.0.0.1

# Regenerate TypeScript bindings after proto edits
gen-proto:
    npm --prefix ui run gen:proto

# Copy chalk-private's permission protos (CHALK_PRIVATE, default ../chalk-private); --check fails on drift
sync-chalk-protos *flags:
    scripts/sync-chalk-protos.sh {{flags}}

# Fail if schema.sql drifted from migrations
schema-check *targets:
    scripts/schema.sh --check {{targets}}

# Rewrite schema snapshots from a fresh migrated database
schema-write *targets:
    scripts/schema.sh --write {{targets}}

# Create a git worktree under .worktrees/ with its own build directory, seeded
# from this checkout's target/ (an APFS copy-on-write clone on macOS: seconds,
# and no extra disk until files diverge). Cargo then uses <worktree>/target on
# its own, so parallel worktrees never share a build lock or artifacts.
worktree name base="main":
    #!/usr/bin/env bash
    set -euo pipefail
    dir=".worktrees/$(printf '%s' "{{name}}" | tr '/' '-')"
    git worktree add "$dir" -b "{{name}}" "{{base}}"
    just seed-target "$dir"
    echo "worktree: $dir   branch: {{name}}"

# Seed <dir>/target (default: this directory) from the main checkout's target/
# so a fresh worktree does not rebuild every dependency. Idempotent: does
# nothing when <dir>/target already exists. Run it inside any worktree another
# tool created (for example Claude Code's .claude/worktrees/*).
seed-target dir=".":
    #!/usr/bin/env bash
    set -euo pipefail
    main="$(git worktree list --porcelain | awk 'NR==1 {print $2}')"
    src="$main/target"
    dst="{{dir}}/target"
    if [ -e "$dst" ]; then
        echo "$dst already exists; nothing to do"
        exit 0
    fi
    if [ ! -d "$src" ]; then
        echo "no $src to seed from; cargo will build from scratch"
        exit 0
    fi
    if cp -Rc "$src" "$dst" 2>/dev/null; then
        echo "cloned $src -> $dst (copy-on-write)"
    else
        cp -R --reflink=auto "$src" "$dst"
        echo "copied $src -> $dst"
    fi
    # The clone carries the main checkout's freshness stamps for this crate, and
    # cp resets their mtimes, so cargo would call the other checkout's compiled
    # crate "fresh" here. Drop only this crate's artifacts; dependencies stay.
    (cd "{{dir}}" && cargo clean -p {{package}})
    echo "cleared {{package}} artifacts; the first build recompiles only this crate"

# Install locked API documentation dependencies (no serving secrets)
docs-install:
    npm --prefix docs-site ci

# Generate API references from Rust and protobuf sources
docs-generate:
    npm --prefix docs-site run generate

# Local API documentation preview, bound to loopback
docs-dev:
    npm --prefix docs-site run dev

# Verify generated references, build static pages, and check local links
docs-check:
    npm --prefix docs-site run check

# Check synthetic documentation examples using the actual router codecs
docs-test-examples:
    cargo test --locked -p {{package}} --test api_documentation_examples

# Serve the built API documentation, bound to loopback
docs-preview:
    npm --prefix docs-site run preview
