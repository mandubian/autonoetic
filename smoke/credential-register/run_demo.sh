#!/usr/bin/env bash
# credential-register smoke: classic case 5 — register a credential through
# the operator gate, then use it gateway-side with header injection.
# See README.md and docs/rfc/classic-harness-usecase-validation.md §3.5.
#
# The demo secret is NOT a real secret — it exists to be provably absent
# from every LLM-visible surface (the leak scan in verdict.py).
#
# Env knobs:
#   CR_MODEL            llm preset model                (default deepseek-v4-flash)
#   CR_PORT             base port                       (default 4388; OFP=+100, mock=+2)
#   CR_MAX_LLM_TOKENS   root-tree token budget          (default 2000000)
#   CR_MAX_LLM_ROUNDS   root-tree LLM rounds            (default 120)
#   CR_MAX_TOOLS        root-tree tool invocations      (default 300)
#   CR_MAX_WALL_SECS    root-tree wall clock            (default 1800)
#   AUTONOETIC_BIN      prebuilt binary path            (default target/debug/autonoetic)
#
# Prerequisites: cargo build -p autonoetic, OPENCODE_API_KEY, python3 (stdlib).
#
# Usage: smoke/credential-register/run_demo.sh
set -euo pipefail

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$DEMO_DIR/../.." && pwd)"
RUN_DIR="$DEMO_DIR/.run"
BIN="${AUTONOETIC_BIN:-$REPO_ROOT/target/debug/autonoetic}"
MODEL="${CR_MODEL:-deepseek-v4-flash}"
PORT="${CR_PORT:-4388}"
OFP_PORT=$((PORT + 100))
MOCK_PORT=$((PORT + 2))
SID="cred-register"
SERVICE="mockweather"
SECRET="cred-demo-secret-7f3a"
MAX_LLM_TOKENS="${CR_MAX_LLM_TOKENS:-2000000}"
MAX_LLM_ROUNDS="${CR_MAX_LLM_ROUNDS:-120}"
MAX_TOOLS="${CR_MAX_TOOLS:-300}"
MAX_WALL="${CR_MAX_WALL_SECS:-1800}"
export AUTONOETIC_SHARED_SECRET="cred-register-demo-secret"

CFG="$RUN_DIR/config.yaml"
DB="$RUN_DIR/agents/.gateway/gateway.db"

log() { printf '==> %s\n' "$*"; }
die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

port_in_use() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }

# ---------------------------------------------------------------- preflight
command -v python3 >/dev/null || die "python3 not found"
for p in "$PORT" "$OFP_PORT" "$MOCK_PORT"; do
  if port_in_use "$p"; then
    die "port $p is already in use. Set CR_PORT to a free base port."
  fi
done
if [ ! -x "$BIN" ]; then
  log "building autonoetic binary (cargo build -p autonoetic)..."
  (cd "$REPO_ROOT" && cargo build -p autonoetic)
fi
[ -x "$BIN" ] || die "autonoetic binary not found at $BIN"
if [ -z "${OPENCODE_API_KEY:-}" ]; then
  log "WARNING: OPENCODE_API_KEY is not set; LLM calls for model '$MODEL' will fail"
fi

# ------------------------------------------------------------- fresh run dir
if [ -d "$RUN_DIR" ]; then
  [ -f "$RUN_DIR/.credential-register-marker" ] || die "$RUN_DIR exists but was not created by this demo; refusing to remove it"
  rm -rf "$RUN_DIR"
fi
mkdir -p "$RUN_DIR/agents"
touch "$RUN_DIR/.credential-register-marker"

# ------------------------------------------------------------- mock service
MOCK_API_KEY="$SECRET" python3 "$DEMO_DIR/mock_service.py" "$MOCK_PORT" \
  >"$RUN_DIR/mock.log" 2>&1 &
MOCK_PID=$!

# ------------------------------------------------------------------- config
cat > "$CFG" <<EOF
agents_dir: "$RUN_DIR/agents"
port: $PORT
ofp_port: $OFP_PORT
http_port: 0
allow_runtime_lock_drift: true
tls: false
node_id: "cred-register"
node_name: "cred-register"
background_scheduler_enabled: true
background_tick_secs: 1
background_min_interval_secs: 1
max_background_due_per_tick: 8
evidence_mode: full
approval_timeout_secs: 900

# opencode/DeepSeek queueing: TTFB can far exceed the default 120s (observed
# up to 256s). Split budgets: long TTFB allowance, default gap budget for
# mid-stream stalls (see smoke/yfinance-factory for the full rationale).
llm_ttfb_timeout_secs: 300

prompt_budget:
  soft_budget_tokens: 150000

llm_presets:
  smart:
    provider: "opencode"
    model: "$MODEL"
    api_key_env: "OPENCODE_API_KEY"
    temperature: 0.2
    context_window_tokens: 262144
  coding:
    provider: "opencode"
    model: "$MODEL"
    api_key_env: "OPENCODE_API_KEY"
    temperature: 0.1
    context_window_tokens: 262144
  research:
    provider: "opencode"
    model: "$MODEL"
    api_key_env: "OPENCODE_API_KEY"
    temperature: 0.3
    context_window_tokens: 262144
  agentic:
    provider: "opencode"
    model: "$MODEL"
    api_key_env: "OPENCODE_API_KEY"
    temperature: 0.0
    context_window_tokens: 262144

root_session_budget:
  max_llm_rounds: $MAX_LLM_ROUNDS
  max_tool_invocations: $MAX_TOOLS
  max_llm_tokens: $MAX_LLM_TOKENS
  max_wall_clock_secs: $MAX_WALL

loop_guard:
  max_loops_without_progress: 10
  max_tool_failures: 8
  max_child_failures: 5

auto_learning:
  curation_schedule: "37 3 * * *"
EOF

log "bootstrapping reference agents into $RUN_DIR/agents"
"$BIN" --config "$CFG" agent bootstrap --from "$REPO_ROOT/agents" >/dev/null

# ------------------------------------------------------------------ gateway
# NO_COLOR: the stderr mirror is ANSI-colored; the redirected gateway.log
# (and the console mirror grep below) needs plain text.
NO_COLOR=1 "$BIN" --config "$CFG" gateway start --daemon >"$RUN_DIR/gateway.log" 2>&1 &
GATEWAY_PID=$!
RESOLVER_PID=""
MIRROR_PID=""

cleanup() {
  [ -n "$RESOLVER_PID" ] && kill "$RESOLVER_PID" 2>/dev/null || true
  [ -n "$MIRROR_PID" ] && kill "$MIRROR_PID" 2>/dev/null || true
  kill "$MOCK_PID" 2>/dev/null || true
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
port_in_use "$MOCK_PORT" || die "mock service did not start (see $RUN_DIR/mock.log)"

# -------------------------------------------------------- console mirror
# Surface operationally interesting lines as they happen: errors/warnings,
# LLM liveness, token usage, credential events.
console_mirror() {
  tail -n +1 -F "$RUN_DIR/gateway.log" 2>/dev/null \
    | grep --line-buffered -E 'ERROR|WARN|autonoetic\.llm:|LLM stream|LLM first byte|llm exchange|credential|LoopGuard|budget_exhausted' \
    | while IFS= read -r line; do printf '[gateway] %s\n' "$line"; done
}
console_mirror &
MIRROR_PID=$!

# --------------------------------------------------- operator auto-resolver
# Approvals are approved — CredentialPrompt approvals additionally carry the
# demo secret via --secret (the non-interactive equivalent of the masked
# operator prompt) and the R++4 confirm phrase read from the approval row
# ("register <service> <credential_id>"). Interactions are answered with
# "proceed autonomously". The loop runs with -e/-pipefail disabled: one
# transient `gateway pending` failure (e.g. SQLite busy against the live
# gateway) must not kill the resolver — a dead resolver means a demo that
# waits MAX_WALL on a gate nobody answers. Failed resolutions land in
# resolver.log with stderr, never silently.
confirm_phrase_for() {
  python3 - "$DB" "$1" <<'PY'
import sqlite3, sys
conn = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
row = conn.execute(
    "SELECT confirm_phrase FROM approvals WHERE request_id = ?",
    (sys.argv[2],)).fetchone()
if row and row[0]:
    print(row[0])
PY
}
auto_resolve() {
  set +e +o pipefail
  while true; do
    "$BIN" --config "$CFG" gateway pending --root-session "$SID" --json 2>>"$RUN_DIR/resolver.log" \
      | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
items = d if isinstance(d, list) else d.get("pending", d.get("items", []))
for p in items:
    row = json.dumps(p)
    is_cred = ("credential" in row.lower()) or ("mockweather" in row.lower())
    print(json.dumps({"kind": p.get("kind"), "id": p.get("id"), "cred": is_cred}))
' 2>/dev/null | while read -r row; do
      kind="$(printf '%s' "$row" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("kind",""))')"
      pid="$(printf '%s' "$row" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("id",""))')"
      cred="$(printf '%s' "$row" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("cred",False))')"
      [ -n "$pid" ] || continue
      case "$kind" in
        approval)
          phrase="$(confirm_phrase_for "$pid")"
          confirm_args=()
          [ -n "$phrase" ] && confirm_args=(--confirm-phrase "$phrase")
          if [ "$cred" = "True" ]; then
            "$BIN" --config "$CFG" gateway approvals approve "$pid" \
              --secret "api_key=$SECRET" \
              "${confirm_args[@]}" \
              --reason "credential-register demo secret entry" \
              >>"$RUN_DIR/resolver.log" 2>&1 \
              || echo "[auto-resolve] credential approve FAILED for $pid" >>"$RUN_DIR/resolver.log"
          else
            "$BIN" --config "$CFG" gateway approvals approve "$pid" \
              "${confirm_args[@]}" \
              --reason "credential-register demo auto-approval" \
              >>"$RUN_DIR/resolver.log" 2>&1 \
              || echo "[auto-resolve] approve FAILED for $pid" >>"$RUN_DIR/resolver.log"
          fi
          ;;
        interaction)
          "$BIN" --config "$CFG" gateway interactions answer "$pid" \
            --text "Proceed autonomously with your best judgment; do not wait for further input." \
            >>"$RUN_DIR/resolver.log" 2>&1 \
            || echo "[auto-resolve] interaction answer FAILED for $pid" >>"$RUN_DIR/resolver.log"
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
# `chat --test-mode` treats each stdin line as ONE event.ingest message —
# task_prompt.txt is already a single block, sent as-is.
log "sending credential task to planner.default (root session: $SID)"
log "constraints: tokens=$MAX_LLM_TOKENS rounds=$MAX_LLM_ROUNDS tools=$MAX_TOOLS wall=${MAX_WALL}s"
"$BIN" --config "$CFG" chat --test-mode --session-id "$SID" planner.default \
  < "$DEMO_DIR/task_prompt.txt" > "$RUN_DIR/reply.txt" 2>&1 || true
log "chat returned; waiting for the session tree to go quiet"
python3 "$DEMO_DIR/verdict.py" wait-done --db "$DB" --sid "$SID" --timeout "$MAX_WALL" || true

# ------------------------------------------------------------------- verdict
log "verdict"
python3 "$DEMO_DIR/verdict.py" verdict \
  --run-dir "$RUN_DIR" --sid "$SID" --secret "$SECRET" --service "$SERVICE" \
  | tee "$RUN_DIR/verdict.txt"

log "artifacts: $RUN_DIR (config, reply, mock.log, resolver.log, verdict.txt, gateway store)"
