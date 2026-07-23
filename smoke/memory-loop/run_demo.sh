#!/usr/bin/env bash
# memory-loop demo: cold session -> post-session digest -> warm session.
#
# Runs the same weathermerge trap task twice against a fresh gateway store:
#   1. COLD run — empty memory store; the agent must discover the trap
#      (main.py fails with EMANIFEST until the seal step runs) by failing.
#   2. Post-session digest extracts lessons into Tier-2 memories.
#   3. WARM run — new root session, identical task; the gateway primes the
#      agent with the digested lessons ("Prior knowledge" block).
#   4. report.py compares failures / EMANIFEST hits / tool calls.
#
# Prerequisites:
#   - cargo build -p autonoetic (or set AUTONOETIC_BIN)
#   - OPENROUTER_API_KEY in the environment (or override MEMORY_LOOP_MODEL
#     with a model served by a provider whose key you have)
#   - python3 (stdlib only)
#
# Usage: smoke/memory-loop/run_demo.sh
set -euo pipefail

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$DEMO_DIR/../.." && pwd)"
RUN_DIR="$DEMO_DIR/.run"
BIN="${AUTONOETIC_BIN:-$REPO_ROOT/target/debug/autonoetic}"
MODEL="${MEMORY_LOOP_MODEL:-minimax/minimax-m2.7}"
TRAP_DIR="$DEMO_DIR/trap-project"
PORT="${MEMORY_LOOP_PORT:-4177}"
OFP_PORT=$((PORT + 100))
COLD_SID="ml-cold"
WARM_SID="ml-warm"
export AUTONOETIC_SHARED_SECRET="memory-loop-demo-secret"

CFG="$RUN_DIR/config.yaml"
DB="$RUN_DIR/agents/.gateway/gateway.db"

log() { printf '==> %s\n' "$*"; }
die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

port_in_use() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }

# ---------------------------------------------------------------- preflight
command -v python3 >/dev/null || die "python3 not found"
for p in "$PORT" "$OFP_PORT"; do
  if port_in_use "$p"; then
    die "port $p is already in use (another gateway running?). Set MEMORY_LOOP_PORT to a free base port."
  fi
done
[ -d "$TRAP_DIR" ] || die "trap project missing at $TRAP_DIR"
if [ ! -x "$BIN" ]; then
  log "building autonoetic binary (cargo build -p autonoetic)..."
  (cd "$REPO_ROOT" && cargo build -p autonoetic)
fi
[ -x "$BIN" ] || die "autonoetic binary not found at $BIN"
if [ -z "${OPENROUTER_API_KEY:-}" ]; then
  log "WARNING: OPENROUTER_API_KEY is not set; LLM calls for model '$MODEL' will fail"
fi

# ------------------------------------------------------------- fresh run dir
if [ -d "$RUN_DIR" ]; then
  [ -f "$RUN_DIR/.memory-loop-marker" ] || die "$RUN_DIR exists but was not created by this demo; refusing to remove it"
  rm -rf "$RUN_DIR"
fi
mkdir -p "$RUN_DIR/agents"
touch "$RUN_DIR/.memory-loop-marker"

# ------------------------------------------------------------------- config
cat > "$CFG" <<EOF
agents_dir: "$RUN_DIR/agents"
port: $PORT
ofp_port: $OFP_PORT
http_port: 0
allow_runtime_lock_drift: true
tls: false
node_id: "memory-loop"
node_name: "memory-loop"
background_scheduler_enabled: true
background_tick_secs: 1
background_min_interval_secs: 1
max_background_due_per_tick: 8
evidence_mode: full

llm_presets:
  smart:
    provider: "openrouter"
    model: "$MODEL"
    temperature: 0.2
    context_window_tokens: 128000
  coding:
    provider: "openrouter"
    model: "$MODEL"
    temperature: 0.1
    context_window_tokens: 128000
  research:
    provider: "openrouter"
    model: "$MODEL"
    temperature: 0.3
    context_window_tokens: 128000
  agentic:
    provider: "openrouter"
    model: "$MODEL"
    temperature: 0.0
    context_window_tokens: 128000

digest_agent:
  enabled: true
  min_turns: 2
  llm_preset: agentic

# Keep the periodic memory-curator out of the demo window: with the default
# schedule ("0 */4 * * *") it fires at e.g. 20:00 UTC sharp and adds LLM
# noise mid-run. 03:37 is outside any demo we run interactively.
auto_learning:
  curation_schedule: "37 3 * * *"
EOF

log "bootstrapping reference agents into $RUN_DIR/agents"
"$BIN" --config "$CFG" agent bootstrap --from "$REPO_ROOT/agents" >/dev/null

# ------------------------------------------------------------------ gateway
# NOTE: `gateway start --daemon` does NOT fork-and-return — the CLI process
# stays in the foreground as the daemon supervisor. Launch it with `&` and
# manage the PID ourselves.
log "starting gateway (port $PORT)"
"$BIN" --config "$CFG" gateway start --daemon >"$RUN_DIR/gateway.log" 2>&1 &
GATEWAY_PID=$!
RESOLVER_PID=""

cleanup() {
  [ -n "$RESOLVER_PID" ] && kill "$RESOLVER_PID" 2>/dev/null || true
  if kill -0 "$GATEWAY_PID" 2>/dev/null; then
    log "stopping gateway (pid $GATEWAY_PID)"
    kill "$GATEWAY_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

GATEWAY_UP=0
for _ in $(seq 1 60); do
  kill -0 "$GATEWAY_PID" 2>/dev/null \
    || die "gateway process died during startup (see $RUN_DIR/gateway.log)"
  if port_in_use "$PORT"; then
    GATEWAY_UP=1
    break
  fi
  sleep 1
done
[ "$GATEWAY_UP" = "1" ] || die "gateway did not start listening on port $PORT (see $RUN_DIR/gateway.log)"

# --------------------------------------------------- approval auto-resolver
# The demo operator is a script: every approval request is approved (root
# scope), every user interaction is answered with "proceed autonomously".
auto_resolve() {
  while true; do
    "$BIN" --config "$CFG" gateway approvals list --json 2>/dev/null \
      | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
items = d if isinstance(d, list) else d.get("approvals", d.get("items", []))
for a in items:
    rid = a.get("request_id")
    status = str(a.get("status", "pending")).lower()
    if rid and status in ("pending", "awaitingapproval", "awaiting_approval"):
        print(rid)
' 2>/dev/null | while read -r rid; do
      "$BIN" --config "$CFG" gateway approvals approve "$rid" \
        --reason "memory-loop demo auto-approval" >/dev/null 2>&1 || true
    done

    "$BIN" --config "$CFG" gateway interactions list --json 2>/dev/null \
      | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
items = d if isinstance(d, list) else d.get("interactions", d.get("items", []))
for i in items:
    iid = i.get("interaction_id")
    status = str(i.get("status", "pending")).lower()
    if iid and status in ("pending", "open"):
        print(iid)
' 2>/dev/null | while read -r iid; do
      "$BIN" --config "$CFG" gateway interactions answer "$iid" \
        --text "Proceed autonomously with your best judgment; do not wait for further input." \
        >/dev/null 2>&1 || true
    done

    sleep 2
  done
}
auto_resolve &
RESOLVER_PID=$!

# --------------------------------------------------------------- task + run
TASK="Copy the Python project named weathermerge from $TRAP_DIR (an absolute host path readable from your sandbox) into your own workspace. Then run main.py to produce report.md from the station data under data/. If a command fails, inspect the project (README.md, Makefile, tools/) to understand why and recover. Work fully autonomously and do not ask questions. The task is done when report.md exists in your workspace copy and contains the merged weather report. Finish by replying with the first 5 lines of report.md."

run_session() {
  local sid="$1" label="$2"
  log "$label run starting (root session: $sid)"
  printf '%s\n' "$TASK" \
    | "$BIN" --config "$CFG" chat --test-mode --session-id "$sid" planner.default \
      > "$RUN_DIR/$sid.reply.txt" 2>&1 || true
  log "$label run: chat returned; waiting for tool activity to settle"
  python3 "$DEMO_DIR/report.py" wait-done --db "$DB" --sid "$sid" --timeout 900 || true
}

# ------------------------------------------------------------------ cold run
run_session "$COLD_SID" "COLD"

log "waiting for post-session digest of the cold run"
if python3 "$DEMO_DIR/report.py" wait-digest --db "$DB" --sid "$COLD_SID" --timeout 300; then
  DIGEST_OK=1
else
  DIGEST_OK=0
  log "WARNING: no digest memories for the cold run — warm run will be unprimed"
fi

# ------------------------------------------------------------------ warm run
run_session "$WARM_SID" "WARM"
python3 "$DEMO_DIR/report.py" wait-digest --db "$DB" --sid "$WARM_SID" --timeout 60 || true

# -------------------------------------------------------------------- report
log "comparison report"
python3 "$DEMO_DIR/report.py" compare --db "$DB" --cold "$COLD_SID" --warm "$WARM_SID" \
  | tee "$RUN_DIR/report.txt"

log "artifacts: $RUN_DIR (config, replies, report, gateway store)"
[ "$DIGEST_OK" = "1" ]
