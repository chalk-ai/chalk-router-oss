# Shared helpers for booting a pinned, throwaway PostgreSQL container.
#
# Sourced by scripts/schema.sh and scripts/test-postgres.sh; this file is not
# meant to be executed directly. Callers are expected to already be running
# under `set -euo pipefail`.

# Digest-pinned so a rebuilt upstream tag cannot silently change a dump.
#
# Public CI and local contributors pull anonymously from Docker Hub. Operators
# can override the image with ROUTER_POSTGRES_SCHEMA_IMAGE for a private mirror.
postgres_image="${ROUTER_POSTGRES_SCHEMA_IMAGE:-postgres:16.14-alpine3.24@sha256:57c72fd2a128e416c7fcc499958864df5301e940bca0a56f58fddf30ffc07777}"

# On a Buildkite agent the Docker-in-Docker socket is owned by root, so every
# docker invocation has to go through sudo (DOCKER_CONFIG carries the Artifact
# Registry credentials the registry-auth plugin wrote, which root would not
# otherwise see). Locally the daemon belongs to the invoking user.
# `wait_for_docker` picks whichever works — and keeps retrying, because dind
# needs a moment to come up before the first command in the job can reach it.
docker_sudo=""
docker_cmd() {
  if [[ -n "${docker_sudo}" ]]; then
    sudo DOCKER_CONFIG="${DOCKER_CONFIG:-}" docker "$@"
  else
    docker "$@"
  fi
}

wait_for_docker() {
  local retries="${DOCKER_WAIT_RETRIES:-60}"
  local attempt
  for attempt in $(seq 1 "${retries}"); do
    if docker info >/dev/null 2>&1; then
      docker_sudo=""
      return 0
    fi
    # -n so a dev box without passwordless sudo fails fast instead of blocking
    # on a password prompt.
    if sudo -n docker info >/dev/null 2>&1; then
      docker_sudo="sudo"
      return 0
    fi
    if [[ "${attempt}" -eq 1 ]]; then
      # stderr: callers that capture this script's stdout as data (a
      # connection URL) must not see progress noise mixed in.
      echo "Waiting for the Docker daemon..." >&2
    fi
    sleep 1
  done
  echo "Docker daemon is not reachable (tried both direct and sudo)" >&2
  return 1
}

# Start a fresh, throwaway PostgreSQL container named "$1" on a random host
# port, wait for it to accept connections, and set POSTGRES_URL to its
# connection string. Every diagnostic goes to stderr, so callers may treat
# POSTGRES_URL (or this function's own stdout, which stays empty) as clean.
# The caller must eventually call stop_postgres_container with the same name.
start_postgres_container() {
  local name="$1"
  docker_cmd rm --force "${name}" >/dev/null 2>&1 || true
  docker_cmd run \
    --detach \
    --rm \
    --name "${name}" \
    --env POSTGRES_PASSWORD=postgres \
    --publish 127.0.0.1::5432 \
    "${postgres_image}" >/dev/null

  # Probe over TCP, matching the endpoint the migration command uses.
  local attempt
  for attempt in $(seq 1 60); do
    if docker_cmd exec "${name}" \
      pg_isready --username postgres --host 127.0.0.1 --port 5432 >/dev/null 2>&1; then
      break
    fi
    if [[ "${attempt}" -eq 60 ]]; then
      docker_cmd logs "${name}" >&2
      echo "PostgreSQL did not become ready" >&2
      return 1
    fi
    sleep 1
  done

  local published_address port
  published_address="$(docker_cmd port "${name}" 5432/tcp)"
  port="${published_address##*:}"
  POSTGRES_URL="postgres://postgres:postgres@127.0.0.1:${port}/postgres?sslmode=disable"
}

stop_postgres_container() {
  local name="$1"
  docker_cmd rm --force "${name}" >/dev/null 2>&1 || true
}
