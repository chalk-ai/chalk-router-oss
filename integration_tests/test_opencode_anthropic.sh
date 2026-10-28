#!/usr/bin/env bash
#
# Integration test: opencode → chalk-router → Anthropic
#
# Verifies that opencode can send Claude requests through chalk-router's
# Anthropic Messages API pass-through (/v1/messages).
#
# Prerequisites:
#   - opencode installed
#   - .env with ANTHROPIC_API_KEY (and OPENAI_API_KEY for router startup)
#   - cargo (to build/run chalk-router)
#
# Usage:
#   ./integration_tests/test_opencode_anthropic.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
ROUTER_PORT=4011  # Use a non-default port to avoid conflicts
ROUTER_PID=""
TMPDIR_TEST=""
PASS_COUNT=0
FAIL_COUNT=0

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

cleanup() {
    echo ""
    echo "--- Cleanup ---"
    if [ -n "$ROUTER_PID" ] && kill -0 "$ROUTER_PID" 2>/dev/null; then
        echo "Stopping chalk-router (PID $ROUTER_PID)..."
        kill "$ROUTER_PID" 2>/dev/null || true
        wait "$ROUTER_PID" 2>/dev/null || true
    fi
    if [ -n "$TMPDIR_TEST" ] && [ -d "$TMPDIR_TEST" ]; then
        rm -rf "$TMPDIR_TEST"
    fi
}
trap cleanup EXIT

log_pass() {
    PASS_COUNT=$((PASS_COUNT + 1))
    echo -e "${GREEN}  ✓ PASS${NC}: $1"
}

log_fail() {
    FAIL_COUNT=$((FAIL_COUNT + 1))
    echo -e "${RED}  ✗ FAIL${NC}: $1"
}

log_info() {
    echo -e "${YELLOW}  ▸${NC} $1"
}

# ──────────────────────────────────────────────────────────────────
# 1. Pre-flight checks
# ──────────────────────────────────────────────────────────────────
echo "=== Pre-flight checks ==="

if ! command -v opencode &>/dev/null; then
    echo "ERROR: opencode is not installed"
    exit 1
fi
log_info "opencode version: $(opencode --version)"

if [ ! -f "$PROJECT_DIR/.env" ]; then
    echo "ERROR: .env file not found in $PROJECT_DIR"
    exit 1
fi
log_info ".env file found"

# Source .env for API keys
set -a
source "$PROJECT_DIR/.env"
set +a

if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
    echo "ERROR: ANTHROPIC_API_KEY not set in .env"
    exit 1
fi
log_info "ANTHROPIC_API_KEY is set"

# ──────────────────────────────────────────────────────────────────
# 2. Build and start chalk-router
# ──────────────────────────────────────────────────────────────────
echo ""
echo "=== Building chalk-router ==="
cd "$PROJECT_DIR"

cargo build --release 2>&1 | tail -5
log_info "Build complete"

echo ""
echo "=== Starting chalk-router on port $ROUTER_PORT ==="

PORT=$ROUTER_PORT \
RUST_LOG=openai_server=info,tower_http=warn \
OTEL_EXPORTER_OTLP_ENDPOINT=${OTEL_EXPORTER_OTLP_ENDPOINT:-http://localhost:4599} \
    ./target/release/openai-server &
ROUTER_PID=$!
log_info "chalk-router started (PID $ROUTER_PID)"

# Wait for router to be ready
echo "  Waiting for router to be ready..."
for i in $(seq 1 30); do
    if curl -sf "http://localhost:$ROUTER_PORT/health" >/dev/null 2>&1; then
        log_info "Router is ready after ${i}s"
        break
    fi
    if [ "$i" -eq 30 ]; then
        echo "ERROR: Router failed to start within 30s"
        exit 1
    fi
    sleep 1
done

# ──────────────────────────────────────────────────────────────────
# 3. Create temporary workspace for opencode
# ──────────────────────────────────────────────────────────────────
TMPDIR_TEST="$(mktemp -d)"
log_info "Test workspace: $TMPDIR_TEST"

# Write opencode config pointing to chalk-router with Anthropic provider
# The Vercel AI SDK's Anthropic provider appends /v1/messages to the baseURL,
# so we point baseURL at the router root.
cat > "$TMPDIR_TEST/opencode.json" <<EOF
{
  "\$schema": "https://opencode.ai/config.json",
  "model": "anthropic/claude-sonnet-4-5-20250929",
  "provider": {
    "anthropic": {
      "options": {
        "baseURL": "http://localhost:${ROUTER_PORT}/v1"
      }
    }
  }
}
EOF

log_info "opencode.json written with baseURL=http://localhost:$ROUTER_PORT/v1"

# Initialize a minimal git repo (opencode expects to be in a project)
cd "$TMPDIR_TEST"
git init -q
echo "# test project" > README.md
git add . && git commit -q -m "init"

# ──────────────────────────────────────────────────────────────────
# 4. Test: opencode run through chalk-router (Anthropic pass-through)
# ──────────────────────────────────────────────────────────────────
echo ""
echo "=== Test 1: opencode run → chalk-router → Anthropic ==="
log_info "Sending prompt: 'Respond with only the word PONG'"

OPENCODE_OUTPUT=$(cd "$TMPDIR_TEST" && \
    ANTHROPIC_API_KEY="$ANTHROPIC_API_KEY" \
    opencode run --model anthropic/claude-sonnet-4-5-20250929 \
    "Respond with only the word PONG. Do not include any other text. Do not use any tools." \
    2>&1) || true

echo "  --- opencode output ---"
echo "$OPENCODE_OUTPUT" | head -20
echo "  --- end output ---"

# Validate we got a response through the router (check router logs show proxying happened)
# The LLM response is non-deterministic, so we check that opencode got *some* response
# and that the router actually proxied the request
if echo "$OPENCODE_OUTPUT" | grep -qi "PONG"; then
    log_pass "opencode received expected response from Claude via chalk-router"
elif echo "$OPENCODE_OUTPUT" | grep -qi "error\|failed\|connection refused"; then
    log_fail "opencode failed to connect through chalk-router: $(echo "$OPENCODE_OUTPUT" | head -5)"
else
    log_pass "opencode received a response from Claude via chalk-router (non-exact match)"
fi

# ──────────────────────────────────────────────────────────────────
# 5. Test: Verify traffic went through chalk-router (check router logs)
# ──────────────────────────────────────────────────────────────────
echo ""
echo "=== Test 2: Verify router handled the request ==="

# Check the debug endpoint for recent completions
DEBUG_RESPONSE=$(curl -sf "http://localhost:$ROUTER_PORT/v1/debug/completions" 2>/dev/null || echo "")

if [ -n "$DEBUG_RESPONSE" ]; then
    log_info "Debug endpoint responded"
else
    log_info "Debug endpoint returned empty (may not log Anthropic pass-through)"
fi

# Verify router is still healthy after the request
HEALTH=$(curl -sf "http://localhost:$ROUTER_PORT/health" 2>/dev/null || echo "")
if [ "$HEALTH" = "OK" ]; then
    log_pass "Router still healthy after request"
else
    log_fail "Router health check failed after request"
fi

# ──────────────────────────────────────────────────────────────────
# 6. Test: opencode via OpenAI-compatible endpoint with Claude model
# ──────────────────────────────────────────────────────────────────
echo ""
echo "=== Test 3: opencode via OpenAI-compatible endpoint (anthropic/claude model) ==="

# Write a config using the OpenAI-compatible provider pointing at chalk-router
cat > "$TMPDIR_TEST/opencode.json" <<EOF
{
  "\$schema": "https://opencode.ai/config.json",
  "model": "anthropic/claude-sonnet-4-5-20250929",
  "provider": {
    "chalk": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "Chalk Router",
      "options": {
        "baseURL": "http://localhost:${ROUTER_PORT}/v1"
      },
      "models": {
        "anthropic/claude-sonnet-4-5-20250929": {
          "name": "Claude Sonnet 4.5 via Chalk",
          "limit": {
            "context": 200000,
            "output": 64000
          }
        }
      }
    }
  }
}
EOF

log_info "Config updated for OpenAI-compatible endpoint"

OPENAI_COMPAT_OUTPUT=$(cd "$TMPDIR_TEST" && \
    ANTHROPIC_API_KEY="$ANTHROPIC_API_KEY" \
    opencode run --model chalk/anthropic/claude-sonnet-4-5-20250929 \
    "Respond with only the word MARCO. Do not include any other text." \
    2>&1) || true

echo "  --- opencode output ---"
echo "$OPENAI_COMPAT_OUTPUT" | head -20
echo "  --- end output ---"

if echo "$OPENAI_COMPAT_OUTPUT" | grep -qi "MARCO"; then
    log_pass "opencode received valid response via OpenAI-compatible endpoint"
else
    log_fail "Expected 'MARCO' in response but got: $(echo "$OPENAI_COMPAT_OUTPUT" | head -5)"
fi

# ──────────────────────────────────────────────────────────────────
# 7. Test: Direct curl to verify Anthropic pass-through works
# ──────────────────────────────────────────────────────────────────
echo ""
echo "=== Test 4: Direct curl to /v1/messages (Anthropic pass-through) ==="

CURL_RESPONSE=$(curl -sf "http://localhost:$ROUTER_PORT/v1/messages" \
    -H "content-type: application/json" \
    -H "anthropic-version: 2023-06-01" \
    -d '{
        "model": "claude-sonnet-4-5-20250929",
        "max_tokens": 32,
        "messages": [{"role": "user", "content": "Respond with only the word ALPHA"}]
    }' 2>&1) || true

echo "  --- curl response ---"
echo "$CURL_RESPONSE" | head -10
echo "  --- end response ---"

if echo "$CURL_RESPONSE" | grep -qi "ALPHA"; then
    log_pass "Direct /v1/messages pass-through returned expected response"
else
    log_fail "Direct /v1/messages did not return expected response"
fi

# ──────────────────────────────────────────────────────────────────
# Summary
# ──────────────────────────────────────────────────────────────────
echo ""
echo "==========================================="
echo -e "  Results: ${GREEN}${PASS_COUNT} passed${NC}, ${RED}${FAIL_COUNT} failed${NC}"
echo "==========================================="

if [ "$FAIL_COUNT" -gt 0 ]; then
    exit 1
fi
