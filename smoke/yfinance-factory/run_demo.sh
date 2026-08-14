#!/usr/bin/env bash
# yfinance-factory smoke: end-to-end agent-creation pipeline under a resource budget.
#
# Launches a fresh gateway with constraints (root_session_budget, loop_guard),
# sends the yfinance-quote factory prompt to planner.default, auto-resolves all
# operator decisions (approvals / interactions), waits for the session tree to
# go quiet, then renders a verdict + improvement proposals via verdict.py.
#
# Constraints (env-overridable):
#   YF_MAX_LLM_TOKENS   root-tree token budget          (default 8000000)
#   YF_MAX_LLM_ROUNDS   root-tree LLM rounds            (default 400)
#   YF_MAX_TOOLS        root-tree tool invocations      (default 1500)
#   YF_MAX_WALL_SECS    root-tree wall clock            (default 3600)
#   YF_MODEL            llm preset model                (default minimax/minimax-m2.7)
#   YF_PORT             base port                       (default 4188)
#   AUTONOETIC_BIN      prebuilt binary path            (default target/debug/autonoetic)
#
# Prerequisites: cargo build -p autonoetic, OPENROUTER_API_KEY, python3 (stdlib).
#
# Usage: smoke/yfinance-factory/run_demo.sh
set -euo pipefail

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$DEMO_DIR/../.." && pwd)"
RUN_DIR="$DEMO_DIR/.run"
BIN="${AUTONOETIC_BIN:-$REPO_ROOT/target/debug/autonoetic}"
MODEL="${YF_MODEL:-minimax/minimax-m2.7}"
PORT="${YF_PORT:-4188}"
OFP_PORT=$((PORT + 100))
SID="yf-factory"
AGENT="yfinance-quote"
MAX_LLM_TOKENS="${YF_MAX_LLM_TOKENS:-8000000}"
MAX_LLM_ROUNDS="${YF_MAX_LLM_ROUNDS:-400}"
MAX_TOOLS="${YF_MAX_TOOLS:-1500}"
MAX_WALL="${YF_MAX_WALL_SECS:-3600}"
export AUTONOETIC_SHARED_SECRET="yf-factory-demo-secret"

CFG="$RUN_DIR/config.yaml"
DB="$RUN_DIR/agents/.gateway/gateway.db"

log() { printf '==> %s\n' "$*"; }
die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

port_in_use() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }

# ---------------------------------------------------------------- preflight
command -v python3 >/dev/null || die "python3 not found"
for p in "$PORT" "$OFP_PORT"; do
  if port_in_use "$p"; then
    die "port $p is already in use. Set YF_PORT to a free base port."
  fi
done
if [ ! -x "$BIN" ]; then
  log "building autonoetic binary (cargo build -p autonoetic)..."
  (cd "$REPO_ROOT" && cargo build -p autonoetic)
fi
[ -x "$BIN" ] || die "autonoetic binary not found at $BIN"
if [ -z "${OPENROUTER_API_KEY:-}" ]; then
  log "WARNING: OPENROUTER_API_KEY is not set; LLM calls for model '$MODEL' will fail"
fi
# Proxy env must be visible to the gateway process so sandboxes inherit it
# (bubblewrap inherits the gateway env; yfinance/requests honor it).
if [ -z "${https_proxy:-}" ]; then
  log "WARNING: https_proxy is not set; Yahoo Finance will be unreachable in a proxy-only env"
fi

# ------------------------------------------------------------- fresh run dir
if [ -d "$RUN_DIR" ]; then
  [ -f "$RUN_DIR/.yfinance-factory-marker" ] || die "$RUN_DIR exists but was not created by this demo; refusing to remove it"
  rm -rf "$RUN_DIR"
fi
mkdir -p "$RUN_DIR/agents"
touch "$RUN_DIR/.yfinance-factory-marker"

# ------------------------------------------------------------------- config
cat > "$CFG" <<EOF
agents_dir: "$RUN_DIR/agents"
port: $PORT
ofp_port: $OFP_PORT
http_port: 0
allow_runtime_lock_drift: true
tls: false
node_id: "yfinance-factory"
node_name: "yfinance-factory"
background_scheduler_enabled: true
background_tick_secs: 1
background_min_interval_secs: 1
max_background_due_per_tick: 8
evidence_mode: full
approval_timeout_secs: 900

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

# Root-tree resource constraints (R+4 / R-6.21) — the tighter of per-session
# and root-tree wins; hitting them ends the session with a budget-exhaustion
# causal event under Ri-0.12, which verdict.py reports.
root_session_budget:
  max_llm_rounds: $MAX_LLM_ROUNDS
  max_tool_invocations: $MAX_TOOLS
  max_llm_tokens: $MAX_LLM_TOKENS
  max_wall_clock_secs: $MAX_WALL

loop_guard:
  max_loops_without_progress: 10
  max_tool_failures: 8
  max_child_failures: 5

# Keep the periodic memory-curator out of the demo window (see memory-loop).
auto_learning:
  curation_schedule: "37 3 * * *"
EOF

log "bootstrapping reference agents into $RUN_DIR/agents"
"$BIN" --config "$CFG" agent bootstrap --from "$REPO_ROOT/agents" >/dev/null

# ------------------------------------------------------------------ gateway
# `gateway start --daemon` stays in the foreground as supervisor — launch with &.
log "starting gateway (port $PORT)"
"$BIN" --config "$CFG" gateway start --daemon >"$RUN_DIR/gateway.log" 2>&1 &
GATEWAY_PID=$!
RESOLVER_PID=""
MIRROR_PID=""

cleanup() {
  [ -n "$RESOLVER_PID" ] && kill "$RESOLVER_PID" 2>/dev/null || true
  [ -n "$MIRROR_PID" ] && kill "$MIRROR_PID" 2>/dev/null || true
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

# -------------------------------------------------------- console mirror
# The gateway logs to $RUN_DIR/gateway.log; surface the operationally
# interesting lines on the demo console as they happen: errors/warnings,
# LLM liveness (first byte, per-stream heartbeats, stalls), per-turn
# token usage. Without this, a long silent LLM turn (overloaded provider)
# is indistinguishable from a dead run.
console_mirror() {
  tail -n +1 -F "$RUN_DIR/gateway.log" 2>/dev/null \
    | grep --line-buffered -E 'ERROR|WARN|autonoetic\.llm:|LLM stream|LLM first byte|llm exchange|LoopGuard|budget_exhausted' \
    | while IFS= read -r line; do printf '[gateway] %s\n' "$line"; done
}
console_mirror &
MIRROR_PID=$!

# --------------------------------------------------- operator auto-resolver
# One unified poll of `gateway pending` (#722): approvals, interactions,
# escalations, plans. Approvals are approved; interactions are answered with
# "proceed autonomously". Anything unresolved is logged for the verdict report.
auto_resolve() {
  while true; do
    "$BIN" --config "$CFG" gateway pending --root-session "$SID" --json 2>/dev/null \
      | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
items = d if isinstance(d, list) else d.get("pending", d.get("items", []))
for p in items:
    print(json.dumps({"kind": p.get("kind"), "id": p.get("id")}))
' 2>/dev/null | while read -r row; do
      kind="$(printf '%s' "$row" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("kind",""))')"
      pid="$(printf '%s' "$row" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("id",""))')"
      [ -n "$pid" ] || continue
      case "$kind" in
        approval)
          "$BIN" --config "$CFG" gateway approvals approve "$pid" \
            --reason "yfinance-factory demo auto-approval" >/dev/null 2>&1 || true
          ;;
        interaction)
          "$BIN" --config "$CFG" gateway interactions answer "$pid" \
            --text "Proceed autonomously with your best judgment; do not wait for further input." \
            >/dev/null 2>&1 || true
          ;;
        *)
          echo "[auto-resolve] unhandled pending kind '$kind' id '$pid'" >>"$RUN_DIR/resolver.log"
          ;;
      esac
    done
    sleep 2
  done
}
auto_resolve &
RESOLVER_PID=$!

# ------------------------------------------------------------------ the run
# `chat --test-mode` treats each stdin line as ONE event.ingest message, so a
# raw multi-line prompt file would arrive as ~55 fragmented turns. Collapse
# the whole factory spec onto a single line (blank lines dropped, structure
# kept inline) so the planner receives the entire spec as ONE block.
PROMPT_ONEBLOCK="$RUN_DIR/factory_prompt.oneblock.txt"
python3 - "$DEMO_DIR/factory_prompt.txt" "$PROMPT_ONEBLOCK" <<'PY'
import sys

src, dst = sys.argv[1], sys.argv[2]
with open(src, encoding="utf-8") as fh:
    text = fh.read()
one = " ".join(part.strip() for part in text.split("\n\n") if part.strip())
one = " ".join(one.split())
with open(dst, "w", encoding="utf-8") as fh:
    fh.write(one + "\n")
PY

log "sending factory prompt to planner.default as one block (root session: $SID)"
log "constraints: tokens=$MAX_LLM_TOKENS rounds=$MAX_LLM_ROUNDS tools=$MAX_TOOLS wall=${MAX_WALL}s"
"$BIN" --config "$CFG" chat --test-mode --session-id "$SID" planner.default \
  < "$PROMPT_ONEBLOCK" > "$RUN_DIR/$SID.reply.txt" 2>&1 || true
log "chat returned; waiting for the session tree to go quiet"
python3 "$DEMO_DIR/verdict.py" wait-done --db "$DB" --sid "$SID" --timeout "$MAX_WALL" || true

# ------------------------------------------------------------------- verdict
log "verdict"
python3 "$DEMO_DIR/verdict.py" verdict \
  --db "$DB" --sid "$SID" --log "$RUN_DIR/gateway.log" \
  --agent "$AGENT" --max-tokens "$MAX_LLM_TOKENS" \
  | tee "$RUN_DIR/verdict.txt"

log "artifacts: $RUN_DIR (config, reply, resolver.log, verdict.txt, gateway store)"
