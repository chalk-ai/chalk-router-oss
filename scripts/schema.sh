#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/schema.sh --check|--write [target...]

Build each router schema from its sqlx migration source in a fresh, pinned
database container and snapshot the result.

  --check  Fail if the committed snapshot differs from the migrated database.
  --write  Replace the committed snapshot with the migrated database schema.

Target:

  default    migrations/default   -> schema.sql         (PostgreSQL)

migrations-clickhouse/ is not covered: it targets ClickHouse, carries no
migration history, and is applied at boot by the usage backend with `{db}`
substituted from config.

Requires Docker; the `default` target also requires Cargo. The digest-pinned
PostgreSQL image is pulled anonymously from Docker Hub. Override it with
ROUTER_POSTGRES_SCHEMA_IMAGE when using a private mirror.
EOF
}

mode=""
case "${1:-}" in
  --check | --write)
    mode="$1"
    shift
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac

targets=("$@")
if [[ "${#targets[@]}" -eq 0 ]]; then
  targets=(default)
fi
for target in "${targets[@]}"; do
  case "${target}" in
    default) ;;
    *)
      echo "unknown target: ${target}" >&2
      usage >&2
      exit 2
      ;;
  esac
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

# shellcheck source=./postgres-lib.sh
source "${repo_root}/scripts/postgres-lib.sh"

run_id="${BUILDKITE_BUILD_ID:-$$}"
temp_dir="$(mktemp -d)"
container_name=""
drifted=0

cleanup() {
  if [[ -n "${container_name}" ]]; then
    docker_cmd rm --force "${container_name}" >/dev/null 2>&1 || true
  fi
  rm -rf "${temp_dir}"
}
trap cleanup EXIT

wait_for_docker

# Start a fresh database, setting the `container_name` global and
# `POSTGRES_URL` (from postgres-lib.sh). Assigns rather than echoing so
# callers cannot invoke it in a command substitution, whose subshell would
# drop the container name and leak the container past the cleanup trap.
start_database() {
  container_name="chalk-router-schema-${run_id}"
  start_postgres_container "${container_name}"
}

stop_database() {
  stop_postgres_container "${container_name}"
  container_name=""
}

# Apply one .sql file per invocation.
apply_sql_file() {
  local file="$1"
  docker_cmd exec --interactive "${container_name}" psql \
    --username postgres --dbname postgres --quiet --no-psqlrc \
    --variable ON_ERROR_STOP=1 --file - <"${file}"
}

# Default migrations are transactional under SQLx. Use the same boundary when
# a migration is applied directly for an upgrade-path fixture.
apply_sql_file_transactionally() {
  local file="$1"
  docker_cmd exec --interactive "${container_name}" psql \
    --username postgres --dbname postgres --quiet --no-psqlrc \
    --variable ON_ERROR_STOP=1 --single-transaction --file - <"${file}"
}

# Exercise the data-bearing upgrade separately from the fresh-schema snapshot.
# The production PostgreSQL ledger already has rows, so migrations 0007-0009
# must preserve them and attribute only cases supported by the old singleton
# rule.
# Apply each migration in its own transaction to reproduce SQLx's lock
# boundaries.
verify_default_usage_backfill() {
  echo "--- :postgres: default — verifying the 0007-0009 usage backfill"
  start_database

  local file
  for file in migrations/default/000{1,2,3,4,5,6}_*.sql; do
    apply_sql_file "${file}"
  done

  docker_cmd exec --interactive "${container_name}" psql \
    --username postgres --dbname postgres --quiet --no-psqlrc \
    --variable ON_ERROR_STOP=1 <<'SQL'
INSERT INTO chalk_router.provider_connections
    (environment_id, id, name, provider_kind, prefix, exposure_policy,
     api_key_envelope, created_at, updated_at)
VALUES
    ('env-a', 'conn-openai', 'OpenAI', 'openai', 'openai/', 'dynamic',
     decode('00', 'hex'), '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z'),
    ('env-a', 'conn-compatible-1', 'Compatible one', 'openai-compatible', 'one/', 'dynamic',
     decode('00', 'hex'), '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z'),
    ('env-a', 'conn-compatible-2', 'Compatible two', 'openai-compatible', 'two/', 'dynamic',
     decode('00', 'hex'), '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z');

INSERT INTO chalk_router.usage_events
    (time, environment_id, api_key_hash, provider, model, total_tokens, request_count)
VALUES
    ('2025-01-01T00:00:00Z', 'env-a', 'key-1', 'openai', 'model', 11, 1),
    ('2023-01-01T00:00:00Z', 'env-a', 'key-2', 'openai', 'model', 13, 1),
    ('2025-01-01T00:00:00Z', 'env-a', 'key-3', 'openai-compatible', 'model', 17, 1),
    ('2025-01-01T00:00:00Z', 'env-a', 'key-4', 'anthropic', 'model', 19, 1);
SQL

  for file in migrations/default/000{7,8,9}_*.sql; do
    apply_sql_file_transactionally "${file}"
  done

  # Simulate an old replica flushing after the migration. It must remain able
  # to omit connection_id during a rolling deployment.
  docker_cmd exec --interactive "${container_name}" psql \
    --username postgres --dbname postgres --quiet --no-psqlrc \
    --variable ON_ERROR_STOP=1 <<'SQL'
INSERT INTO chalk_router.usage_events
    (time, environment_id, api_key_hash, provider, model, total_tokens, request_count)
VALUES
    ('2025-01-02T00:00:00Z', 'env-a', 'key-5', 'openai', 'model', 23, 1);
SQL

  for file in migrations/default/001{0,1,2}_*.sql; do
    apply_sql_file_transactionally "${file}"
  done

  docker_cmd exec --interactive "${container_name}" psql \
    --username postgres --dbname postgres --quiet --no-psqlrc \
    --variable ON_ERROR_STOP=1 <<'SQL'
DO $verify$
BEGIN
  IF (SELECT count(*) FROM chalk_router.usage_events) <> 5 THEN
    RAISE EXCEPTION '0007-0009 did not preserve every usage row';
  END IF;
  IF (SELECT sum(total_tokens) FROM chalk_router.usage_events) <> 83 THEN
    RAISE EXCEPTION '0007-0009 changed usage totals';
  END IF;
  IF (SELECT connection_id FROM chalk_router.usage_events WHERE api_key_hash = 'key-1')
       IS DISTINCT FROM 'conn-openai' THEN
    RAISE EXCEPTION '0008 did not backfill the unambiguous built-in event';
  END IF;
  IF (SELECT count(*) FROM chalk_router.usage_events WHERE connection_id IS NULL) <> 4 THEN
    RAISE EXCEPTION '0008 attributed an ambiguous or rolling-deployment event';
  END IF;
  IF (SELECT is_nullable
        FROM information_schema.columns
       WHERE table_schema = 'chalk_router'
         AND table_name = 'usage_events'
         AND column_name = 'connection_id') IS DISTINCT FROM 'YES' THEN
    RAISE EXCEPTION 'usage_events.connection_id must remain nullable for legacy rows';
  END IF;
  IF to_regclass('chalk_router.uq_router_provider_connections_builtin_kind') IS NOT NULL THEN
    RAISE EXCEPTION '0009 did not remove the built-in provider singleton index';
  END IF;
END
$verify$;
SQL

  apply_sql_file_transactionally migrations/default/0013_add_usage_event_identity.sql
  apply_sql_file_transactionally migrations/default/0014_backfill_usage_event_identity.sql
  apply_sql_file migrations/default/0015_usage_event_identity_index.sql
  apply_sql_file migrations/default/0016_usage_connection_time_index.sql
  apply_sql_file_transactionally migrations/default/0017_finalize_usage_events.sql

  docker_cmd exec --interactive "${container_name}" psql \
    --username postgres --dbname postgres --quiet --no-psqlrc \
    --variable ON_ERROR_STOP=1 <<'SQL'
DO $verify$
BEGIN
  IF (SELECT count(DISTINCT event_id) FROM chalk_router.usage_events) <> 5 THEN
    RAISE EXCEPTION '0013-0017 did not assign a unique event ID to every historical row';
  END IF;
  IF (SELECT count(*) FROM chalk_router.usage_events
       WHERE usage_source = 3) <> 5 THEN
    RAISE EXCEPTION '0013-0017 did not mark historical usage provenance as unknown';
  END IF;
  IF (SELECT is_nullable
        FROM information_schema.columns
       WHERE table_schema = 'chalk_router'
         AND table_name = 'usage_events'
         AND column_name = 'event_id') IS DISTINCT FROM 'NO' THEN
    RAISE EXCEPTION '0017 did not finalize event_id as NOT NULL';
  END IF;
  IF to_regclass('chalk_router.uq_router_usage_events_event') IS NULL THEN
    RAISE EXCEPTION '0015 did not create the event identity index';
  END IF;
  IF to_regclass('chalk_router.idx_router_usage_events_connection_time') IS NULL THEN
    RAISE EXCEPTION '0016 did not create the connection/time index';
  END IF;
END
$verify$;
SQL
  stop_database
}

# Dump the chalk_router schema as the canonical snapshot.
#
# `\restrict`/`\unrestrict` guards are stripped: they exist to make a dump safe
# to feed to an untrusted psql, and their presence varies by pg_dump patch
# level, which would churn the snapshot across image versions.
# Trailing blank lines are trimmed so the file ends in exactly one newline.
dump_schema() {
  local source_dir="$1"
  echo "-- Generated by scripts/schema.sh --write; do not edit by hand."
  echo "-- Canonical source: ${source_dir}/"
  echo
  docker_cmd exec "${container_name}" pg_dump \
    --username postgres \
    --dbname postgres \
    --schema-only \
    --schema chalk_router \
    --exclude-table chalk_router._sqlx_migrations \
    --exclude-table chalk_router._router_migration_lease \
    --no-owner \
    --no-privileges |
    grep -v -E '^\\(un)?restrict ' |
    awk '
      NF {
        while (pending_blank_lines > 0) {
          print ""
          pending_blank_lines--
        }
        print
        next
      }
      { pending_blank_lines++ }
    '
}

# Compare the freshly dumped schema against the committed snapshot, or
# overwrite it under --write.
settle() {
  local schema_file="$1" candidate="$2"
  if [[ "${mode}" == "--write" ]]; then
    cp "${candidate}" "${schema_file}"
    echo "Updated ${schema_file#"${repo_root}/"}"
    return
  fi
  if [[ ! -f "${schema_file}" ]]; then
    echo "${schema_file#"${repo_root}/"} is missing." >&2
    echo "Run scripts/schema.sh --write and commit the result." >&2
    drifted=1
    return
  fi
  if ! diff --unified "${schema_file}" "${candidate}"; then
    echo
    echo "${schema_file#"${repo_root}/"} is out of date." >&2
    echo "Run scripts/schema.sh --write and commit the result." >&2
    drifted=1
    return
  fi
  echo "${schema_file#"${repo_root}/"} matches the migrated database"
}

build_default() {
  verify_default_usage_backfill

  echo "--- :postgres: default — starting fresh PostgreSQL"
  start_database

  # Applied with the production command, so the snapshot also exercises
  # qualified SQLx history, the transaction-pool-safe lease, and the
  # invalid-index guard in migrate.rs.
  echo "--- :database: default — applying migrations via \`openai-server migrate\`"
  ROUTER_DATABASE_URL="${POSTGRES_URL}" \
  CHALK_ENVIRONMENT_ID=schema-check \
    cargo run --locked --quiet --bin openai-server -- migrate

  echo "--- :database: default — dumping canonical schema"
  dump_schema migrations/default >"${temp_dir}/schema.sql"
  stop_database
  settle "${repo_root}/schema.sql" "${temp_dir}/schema.sql"
}

for target in "${targets[@]}"; do
  "build_${target}"
done

if [[ "${drifted}" -ne 0 ]]; then
  exit 1
fi
