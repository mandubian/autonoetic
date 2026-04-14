#!/usr/bin/env bash
# examples/moltbook_registration/run.sh
#
# Demonstrates the external-service registration workflow with human intervention.
#
# What it runs:
#   1. Mock Moltbook server (port 8765) — simulates an external platform
#   2. Autonoetic gateway (port 4100)
#   3. Interactive terminal chat — the agent walks through the registration flow,
#      pausing at two points to ask the operator for input:
#        - X/Twitter username
#        - URL of the verification tweet
#
# Prerequisites:
#   OPENROUTER_API_KEY must be set (or set MODE=smoke to skip LLM calls).
#
# Usage:
#   bash examples/moltbook_registration/run.sh [WORKDIR] [AGENT_ID] [MODE]
#
#   WORKDIR   — scratch directory  (default: /tmp/autonoetic-moltbook)
#   AGENT_ID  — agent install name (default: moltbook_demo)
#   MODE      — openrouter | smoke (default: openrouter)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

WORKDIR="${1:-/tmp/autonoetic-moltbook}"
AGENT_ID="${2:-moltbook_demo}"
MODE="${3:-openrouter}"

CONFIG_PATH="${WORKDIR}/config.yaml"
AGENTS_DIR="${WORKDIR}/agents"
AGENT_DIR="${AGENTS_DIR}/${AGENT_ID}"
SKILL_PATH="${AGENT_DIR}/SKILL.md"
RUNTIME_LOCK_PATH="${AGENT_DIR}/runtime.lock"
SESSION_ID="moltbook-session-${AGENT_ID}"
CHANNEL_ID="terminal:moltbook:${AGENT_ID}"
GATEWAY_PORT=4100
MOLTBOOK_PORT=8765

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

wait_for_port() {
  local host="$1" port="$2" retries="${3:-50}"
  for ((i=0; i<retries; i++)); do
    if (echo >"/dev/tcp/${host}/${port}") >/dev/null 2>&1; then return 0; fi
    sleep 0.2
  done
  echo "ERROR: timed out waiting for ${host}:${port}" >&2
  return 1
}

cleanup() {
  if [[ -n "${GATEWAY_PID:-}" ]] && kill -0 "${GATEWAY_PID}" 2>/dev/null; then
    kill "${GATEWAY_PID}" 2>/dev/null || true
    wait "${GATEWAY_PID}" 2>/dev/null || true
  fi
  if [[ -n "${MOLTBOOK_PID:-}" ]] && kill -0 "${MOLTBOOK_PID}" 2>/dev/null; then
    kill "${MOLTBOOK_PID}" 2>/dev/null || true
    wait "${MOLTBOOK_PID}" 2>/dev/null || true
  fi
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Validate prerequisites
# ---------------------------------------------------------------------------

if [[ "${MODE}" == "openrouter" && -z "${OPENROUTER_API_KEY:-}" ]]; then
  echo "ERROR: OPENROUTER_API_KEY is required for mode=openrouter" >&2
  echo "  Set it and re-run, or use mode=smoke for a startup-only check." >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Prepare workspace
# ---------------------------------------------------------------------------

mkdir -p "${WORKDIR}"

cat > "${CONFIG_PATH}" <<EOF
agents_dir: "${AGENTS_DIR}"
port: ${GATEWAY_PORT}
ofp_port: $((GATEWAY_PORT + 200))
tls: false
EOF

# ---------------------------------------------------------------------------
# Install agent
# ---------------------------------------------------------------------------

cd "${PROJECT_ROOT}"

if [[ -d "${AGENT_DIR}" && "${AUTONOETIC_RESET:-0}" == "1" ]]; then
  echo "==> Resetting existing agent '${AGENT_ID}'"
  rm -rf "${AGENT_DIR}"
fi

if [[ -d "${AGENT_DIR}" ]]; then
  echo "==> Reusing existing agent '${AGENT_ID}' at ${AGENT_DIR}"
else
  echo "==> Installing agent '${AGENT_ID}'"
  mkdir -p "${AGENT_DIR}/state" "${AGENT_DIR}/history" "${AGENT_DIR}/skills" "${AGENT_DIR}/scripts"
  cp "${SCRIPT_DIR}/sample_agent/SKILL.md" "${SKILL_PATH}"
  cp "${SCRIPT_DIR}/sample_agent/runtime.lock" "${RUNTIME_LOCK_PATH}"
  sed -i "s/__AGENT_ID__/${AGENT_ID}/g" "${SKILL_PATH}"
fi

if [[ "${MODE}" == "openrouter" ]]; then
  sed -i 's/provider: ".*"/provider: "openrouter"/' "${SKILL_PATH}"
  sed -i 's/model: ".*"/model: "google\/gemini-3-flash-preview"/' "${SKILL_PATH}"
fi

# ---------------------------------------------------------------------------
# Build binaries
# ---------------------------------------------------------------------------

echo "==> Building binaries"
cargo build --bin autonoetic --bin mock-moltbook -p autonoetic -p autonoetic-gateway 2>&1 \
  | grep -E "(Compiling|Finished|error)" || true

# ---------------------------------------------------------------------------
# Start Mock Moltbook
# ---------------------------------------------------------------------------

echo "==> Starting Mock Moltbook server on port ${MOLTBOOK_PORT}"
MOCK_MOLTBOOK_PORT="${MOLTBOOK_PORT}" \
  cargo run --bin mock-moltbook -p autonoetic-gateway -- \
  > "${WORKDIR}/moltbook.log" 2>&1 &
MOLTBOOK_PID=$!

wait_for_port 127.0.0.1 "${MOLTBOOK_PORT}"
echo "    Mock Moltbook ready (PID ${MOLTBOOK_PID})"

# ---------------------------------------------------------------------------
# Start gateway
# ---------------------------------------------------------------------------

echo "==> Starting gateway on port ${GATEWAY_PORT}"
export AUTONOETIC_NODE_ID="${AUTONOETIC_NODE_ID:-moltbook-demo-node}"
export AUTONOETIC_NODE_NAME="${AUTONOETIC_NODE_NAME:-Moltbook Demo Gateway}"
export AUTONOETIC_SHARED_SECRET="${AUTONOETIC_SHARED_SECRET:-moltbook-demo-secret}"
[[ -n "${OPENROUTER_API_KEY:-}" ]] && export OPENROUTER_API_KEY

cargo run -p autonoetic -- --config "${CONFIG_PATH}" gateway start \
  > "${WORKDIR}/gateway.log" 2>&1 &
GATEWAY_PID=$!

wait_for_port 127.0.0.1 "${GATEWAY_PORT}"
echo "    Gateway ready (PID ${GATEWAY_PID})"

# ---------------------------------------------------------------------------
# Run chat
# ---------------------------------------------------------------------------

echo
echo "============================================================"
echo "  Moltbook Registration Demo"
echo "============================================================"
echo
echo "The agent will:"
echo "  1. Register with Mock Moltbook (automatic)"
echo "  2. Ask you for your X/Twitter username"
echo "  3. Give you a tweet to post, then ask for the tweet URL"
echo "  4. Complete verification and set up heartbeat"
echo "  5. Post an inaugural message to the feed"
echo
echo "Mock Moltbook status: http://localhost:${MOLTBOOK_PORT}/status"
echo "Gateway log:          ${WORKDIR}/gateway.log"
echo "Moltbook log:         ${WORKDIR}/moltbook.log"
echo
echo "Type '/exit' to quit the chat at any time."
echo "============================================================"
echo

if [[ "${MODE}" == "smoke" ]]; then
  printf 'Start the Moltbook registration workflow.\n/exit\n' \
    | cargo run -p autonoetic -- --config "${CONFIG_PATH}" chat "${AGENT_ID}" \
        --sender-id demo \
        --channel-id "${CHANNEL_ID}" \
        --session-id "${SESSION_ID}"
else
  cargo run -p autonoetic -- --config "${CONFIG_PATH}" chat "${AGENT_ID}" \
    --sender-id demo \
    --channel-id "${CHANNEL_ID}" \
    --session-id "${SESSION_ID}"
fi

echo
echo "============================================================"
echo "Demo complete."
echo
echo "Inspect mock server state:"
echo "  curl http://localhost:${MOLTBOOK_PORT}/status | python3 -m json.tool"
echo
echo "Inspect agent memory:"
echo "  cat ${AGENT_DIR}/state/moltbook_registration.json"
echo
echo "Inspect session traces:"
echo "  cargo run -p autonoetic -- --config \"${CONFIG_PATH}\" trace show \"${SESSION_ID}\" --agent \"${AGENT_ID}\""
echo "============================================================"
