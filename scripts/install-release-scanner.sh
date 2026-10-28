#!/usr/bin/env bash
# Install one checksum-pinned scanner into the requested temporary directory.
set -euo pipefail
out="${1:?usage: install-release-scanner.sh OUTPUT_DIRECTORY}"
test "$(uname -s)" = Linux
test "$(uname -m)" = x86_64
mkdir -p "$out"
curl --fail --silent --show-error --location --retry 3 \
  https://github.com/trufflesecurity/trufflehog/releases/download/v3.99.7/trufflehog_3.99.7_linux_amd64.tar.gz \
  --output "$out/scanner.tar.gz"
(cd "$out" && echo 'bdbea0508e439b144f49de14e775908de5f902f3d14ea7ae8354bae72b0b72ef  scanner.tar.gz' | sha256sum --check)
tar -xzf "$out/scanner.tar.gz" -C "$out" trufflehog
"$out/trufflehog" --version
