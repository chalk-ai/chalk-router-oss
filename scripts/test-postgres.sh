#!/usr/bin/env bash
set -euo pipefail

# Boot or tear down a throwaway PostgreSQL container for local Postgres-gated
# testing, without touching a shared database. This is the local equivalent
# of the database setup in .github/workflows/ci.yml.
#
#   scripts/test-postgres.sh start [name]   # prints only the connection URL
#   scripts/test-postgres.sh stop [name]

usage() {
  cat <<'EOF'
Usage: scripts/test-postgres.sh start|stop [name]

  start  Boot a fresh PostgreSQL container and print its connection URL on
         stdout (nothing else is written to stdout). name defaults to
         chalk-router-test-pg.
  stop   Remove the container.

Requires Docker.
EOF
}

action="${1:-}"
name="${2:-chalk-router-test-pg}"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=./postgres-lib.sh
source "${repo_root}/scripts/postgres-lib.sh"

case "${action}" in
  start)
    wait_for_docker
    start_postgres_container "${name}"
    echo "${POSTGRES_URL}"
    ;;
  stop)
    stop_postgres_container "${name}"
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
