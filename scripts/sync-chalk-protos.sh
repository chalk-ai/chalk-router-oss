#!/usr/bin/env bash
# Copy the Chalk protos this repo reads from chalk-private, byte for byte.
#
#   scripts/sync-chalk-protos.sh           copy from $CHALK_PRIVATE (default ../chalk-private)
#   scripts/sync-chalk-protos.sh --check   fail if the copies differ from that checkout
#
# permissions.proto supplies the (chalk.auth.v1.permission) options that
# build.rs turns into the admin RPC permission table; encoding.proto is its
# only Chalk import. Never edit the copies here: change chalk-private, then sync.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
source_root="${CHALK_PRIVATE:-$root/../chalk-private}/protos/protos"
files=(
  chalk/auth/v1/permissions.proto
  chalk/utils/v1/encoding.proto
)

if [[ ! -d "$source_root" ]]; then
  echo "sync-chalk-protos: $source_root not found; set CHALK_PRIVATE to a chalk-private checkout" >&2
  exit 1
fi

mode="${1:-}"
status=0
for file in "${files[@]}"; do
  if [[ "$mode" == "--check" ]]; then
    if ! cmp -s "$source_root/$file" "$root/proto/$file"; then
      echo "sync-chalk-protos: proto/$file differs from chalk-private" >&2
      status=1
    fi
  else
    mkdir -p "$(dirname "$root/proto/$file")"
    cp "$source_root/$file" "$root/proto/$file"
  fi
done
exit "$status"
