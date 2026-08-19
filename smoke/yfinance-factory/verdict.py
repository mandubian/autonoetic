#!/usr/bin/env python3
"""yfinance-factory smoke: session wait + verdict/improvement report.

Subcommands:
  wait-done   Block until the session tree's tool activity goes quiet AND no
              workflow tasks remain running/queued.
  verdict     Inspect the gateway store + gateway.log, decide PASS/FAIL/
              INCONCLUSIVE, and print signature-matched improvement proposals.

Reads the gateway SQLite store (<agents_dir>/.gateway/gateway.db) read-only
with the stdlib sqlite3 module; token accounting is parsed from the gateway
log's `llm exchange` lines (runtime/lifecycle.rs).
"""

from __future__ import annotations

import argparse
import re
import sqlite3
import sys
import time
from collections import Counter

POLL_SECS = 4.0
STABLE_POLLS = 8  # 8 * 4s = 32s of silence, plus no running/queued tasks

LLM_LINE = re.compile(
    r"llm exchange agent_id=(\S+) session_id=(\S+) model=(\S+) "
    r"input_tokens=(\d+) output_tokens=(\d+)"
)

SIGNATURES = [
    ("network_isolated", re.compile(r"network_isolated|Errno 101|Network is unreachable")),
    ("yahoo_429", re.compile(r"\b429\b|rate.?limit", re.IGNORECASE)),
    ("loop_guard", re.compile(r"LoopGuard tripped", re.IGNORECASE)),
    ("budget_exhausted", re.compile(r"budget.?exhaust|max_llm_tokens|max_tool_invocations", re.IGNORECASE)),
    ("approval_timeout", re.compile(r"approval_timeout|auto.?fail", re.IGNORECASE)),
]

IMPROVEMENTS = {
    "network_isolated": (
        "Environmental egress: the target host is unreachable from the sandbox. "
        "Confirm the gateway runs with http_proxy/https_proxy exported (bubblewrap "
        "inherits the gateway env) and that the host is reachable through the proxy "
        "(curl https://query1.finance.yahoo.com). Approval cannot create connectivity."
    ),
    "yahoo_429": (
        "Yahoo rate limiting hit during a live run. The script's retry/backoff is "
        "either missing or too weak — extend it (more attempts, jitter), or make the "
        "smoke input lighter. Hermetic gate tests are unaffected (they mock yfinance)."
    ),
    "loop_guard": (
        "LoopGuard tripped — the pipeline repeated work without progress. Inspect "
        "the trip reason; if the same child failed identically N times, the factory "
        "needs fail-fast on identical failure signatures (or lower max_child_failures)."
    ),
    "budget_exhausted": (
        "The root-session budget was consumed before completion. Raise "
        "YF_MAX_LLM_TOKENS / YF_MAX_TOOLS for a retry, or shrink the task. The "
        "per-agent token table below shows where the budget went."
    ),
    "approval_timeout": (
        "An approval auto-failed on timeout. The auto-resolver polls every 2s — "
        "if this fired, check resolver.log for unhandled pending kinds, or raise "
        "approval_timeout_secs in the demo config."
    ),
}


def connect(db_path: str) -> sqlite3.Connection:
    conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True, timeout=10)
    conn.row_factory = sqlite3.Row
    return conn


def subtree_rows(conn: sqlite3.Connection, table: str, sid: str, cols: str = "*"):
    try:
        return list(conn.execute(
            f"SELECT {cols} FROM {table} "
            "WHERE session_id = ? OR session_id LIKE ? || '/%'",
            (sid, sid),
        ))
    except sqlite3.OperationalError:
        return []


def trace_count(conn: sqlite3.Connection, sid: str) -> int:
    return len(subtree_rows(conn, "execution_traces", sid, "trace_id"))


def active_tasks(conn: sqlite3.Connection, sid: str) -> list[sqlite3.Row]:
    rows = subtree_rows(conn, "task_runs", sid, "task_id, agent_id, status")
    return [r for r in rows if r["status"] in ("running", "queued")]


def pending_approvals(conn: sqlite3.Connection, sid: str) -> list:
    """Open operator gates for the root-session subtree. A task suspended at
    an approval gate is not 'quiet' — the run is not done until every gate
    is resolved (approved/rejected/expired/cancelled). Backported from the
    credential-register verdict (#1109)."""
    try:
        return list(conn.execute(
            "SELECT request_id FROM approvals "
            "WHERE status = 'pending' AND "
            "(root_session_id = ? OR root_session_id LIKE ? || '/%' "
            " OR session_id = ? OR session_id LIKE ? || '/%')",
            (sid, sid, sid, sid)))
    except sqlite3.OperationalError:
        return []


def unadjudicated_flags(conn: sqlite3.Connection, sid: str) -> list:
    """Anomaly flags still open in the root-session subtree: machinery filed
    a complaint nobody has adjudicated (status not in the terminal set
    `confirmed`/`dismissed`/`deferred`). A study verdict that PASSes while
    its own grievances sit unread is the rubber-stamp pattern the RFCs warn
    about, so they surface as a FAIL-worthy section (#1108/3)."""
    terminal = ("confirmed", "dismissed", "deferred")
    ph = ",".join("?" * len(terminal))
    try:
        return list(conn.execute(
            f"SELECT flag_id, reporter_agent_id, severity, status, observation "
            f"FROM anomaly_flags WHERE status NOT IN ({ph}) AND "
            "(reporter_session_id = ? OR reporter_session_id LIKE ? || '/%') "
            "ORDER BY created_at ASC",
            (*terminal, sid, sid)))
    except sqlite3.OperationalError:
        return []


def wait_done(db_path: str, sid: str, timeout: int) -> int:
    deadline = time.monotonic() + timeout
    last = -1
    stable = 0
    while time.monotonic() < deadline:
        try:
            with connect(db_path) as conn:
                n = trace_count(conn, sid)
                active = len(active_tasks(conn, sid))
                gates = len(pending_approvals(conn, sid))
        except sqlite3.Error:
            n, active, gates = last, 1, 1  # transient lock; keep waiting
        if n > 0 and n == last and active == 0 and gates == 0:
            stable += 1
        else:
            stable = 0
        last = n
        if stable >= STABLE_POLLS:
            print(f"[wait-done] {sid}: {n} traces, no running/queued tasks, "
f"no open gates, quiet for {STABLE_POLLS} polls — done",
                  flush=True)
            return 0
        print(f"[wait-done] {sid}: traces={n} active_tasks={active} "
              f"pending_gates={gates} (waiting)", flush=True)
        time.sleep(POLL_SECS)
    print(f"[wait-done] TIMEOUT waiting for session {sid}", flush=True)
    return 1


def token_report(log_path: str, sid: str) -> tuple[Counter, int, int]:
    per_agent: Counter = Counter()
    total_in = total_out = 0
    try:
        with open(log_path, encoding="utf-8", errors="replace") as fh:
            for line in fh:
                m = LLM_LINE.search(line)
                if not m:
                    continue
                agent, sess, _model, tin, tout = m.groups()
                if sess != sid and not sess.startswith(sid + "/"):
                    continue
                tin, tout = int(tin), int(tout)
                per_agent[agent] += tin + tout
                total_in += tin
                total_out += tout
    except FileNotFoundError:
        pass
    return per_agent, total_in, total_out


def signatures(failures: list[str]) -> set[str]:
    found = set()
    for text in failures:
        for name, rx in SIGNATURES:
            if rx.search(text):
                found.add(name)
    return found


def verdict(db_path: str, sid: str, log_path: str, agent: str, max_tokens: int) -> int:
    with connect(db_path) as conn:
        tasks = subtree_rows(conn, "task_runs", sid)
        traces = subtree_rows(conn, "execution_traces", sid)
        try:
            revisions = list(conn.execute(
                "SELECT revision_id, status, created_at FROM agent_revisions "
                "WHERE agent_id = ? ORDER BY created_at", (agent,)))
        except sqlite3.OperationalError:
            revisions = []
        try:
            promotions = list(conn.execute(
                "SELECT kind, new_revision_id, created_at FROM promotion_history "
                "WHERE agent_id = ?", (agent,)))
        except sqlite3.OperationalError:
            promotions = []
        try:
            attempts = list(conn.execute(
                "SELECT outcome, gate, error_code FROM promotion_attempts"))
        except sqlite3.OperationalError:
            attempts = []
        try:
            approvals = list(conn.execute(
                "SELECT action_type, status, reason FROM approvals"))
        except sqlite3.OperationalError:
            approvals = []
        flags = unadjudicated_flags(conn, sid)

    smoke = [t for t in tasks if t["agent_id"] == agent]
    smoke_ok = any(t["status"] == "succeeded" for t in smoke)
    task_status = Counter(t["status"] for t in tasks)
    failed_texts = [
        f"{t.get('result_summary') or ''}"
        for t in tasks if t["status"] == "failed"
    ] + [
        f"{r.get('error_summary') or ''} {r.get('stderr') or ''}"
        for r in traces if r["success"] == 0
    ]
    sigs = signatures(failed_texts)
    per_agent, tin, tout = token_report(log_path, sid)
    promoted = bool(promotions) or any(
        r["status"] in ("promoted", "active") for r in revisions)
    rejected_attempts = [a for a in attempts if a["outcome"] == "rejected"]

    print("=" * 64)
    print("yfinance-factory smoke verdict")
    print("=" * 64)
    print(f"workflow tasks:  {dict(task_status)}")
    print(f"tool traces:     {len(traces)}")
    print(f"smoke runs ({agent}): {len(smoke)} "
          f"({'1 succeeded' if smoke_ok else 'none succeeded'})")
    print(f"promotion:       {'PROMOTED' if promoted else 'not promoted'}")
    if revisions:
        for r in revisions:
            print(f"  revision {r['revision_id'][:24]}… status={r['status']}")
    if rejected_attempts:
        gates = Counter(
            f"{a['gate'] or '?'}:{a['error_code'] or '?'}" for a in rejected_attempts)
        print(f"rejected promotion attempts: {dict(gates)}")
    if approvals:
        print(f"approvals: {dict(Counter(a['action_type'] + '/' + a['status'] for a in approvals))}")
    if flags:
        print("UNADJUDICATED FLAGS (complaints filed, nobody read them):")
        for f in flags:
            print(f"  {f['flag_id']} [{f['severity']}/{f['status']}] "
                  f"by {f['reporter_agent_id']}: {f['observation']}")
    else:
        print("unadjudicated anomaly flags: none")
    print()
    print(f"tokens (root tree): {tin + tout:,} used / {max_tokens:,} cap "
          f"({tin:,} in, {tout:,} out)")
    for name, toks in per_agent.most_common(5):
        print(f"  {name:<32} {toks:>12,}")
    print()

    print("-" * 64)
    print("VERDICT")
    print("-" * 64)
    if not traces:
        print("INCONCLUSIVE: no tool activity was recorded — the task never ran.")
        rc = 2
    elif flags:
        print("FAIL: machinery filed un-adjudicated anomaly flags (listed above) "
              "that nobody reviewed — a PASS would rubber-stamp unread complaints.")
        rc = 1
    elif "budget_exhausted" in sigs and not promoted:
        print("FAIL: the root-session budget was exhausted before promotion.")
        rc = 1
    elif promoted and smoke_ok:
        print("PASS: pipeline completed — gates passed, live smoke succeeded, "
              "revision promoted, within budget.")
        rc = 0
    elif smoke and not smoke_ok:
        print(f"FAIL: {len(smoke)} smoke run(s) and none succeeded — promotion "
              "evidence could not be produced.")
        rc = 1
    elif rejected_attempts and not promoted:
        print("FAIL: promotion was attempted and rejected (see gates above).")
        rc = 1
    else:
        print("INCONCLUSIVE: the pipeline did not reach promotion within the "
              "wall-clock window.")
        rc = 2

    dupes = {text: n for text, n in Counter(failed_texts).most_common(3) if n > 2}
    if sigs or dupes or rc != 0:
        print()
        print("IMPROVEMENTS")
        print("-" * 64)
        for name in sorted(sigs):
            print(f"[{name}] {IMPROVEMENTS[name]}")
        for text, n in dupes.items():
            head = " ".join(text.split())[:100]
            print(f"[repeated_failure ×{n}] Identical failure repeated — the "
                  f"pipeline lacks fail-fast on this signature: {head}…")
        if rc == 2 and not sigs and not dupes:
            print("The pipeline stalled without a recognizable failure signature. "
                  "Check .run/gateway.log tail and `autonoetic gateway pending "
                  f"--root-session {sid}` for un-answered operator decisions.")
    return rc


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    sub = ap.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("wait-done")
    p.add_argument("--db", required=True)
    p.add_argument("--sid", required=True)
    p.add_argument("--timeout", type=int, default=3600)

    p = sub.add_parser("verdict")
    p.add_argument("--db", required=True)
    p.add_argument("--sid", required=True)
    p.add_argument("--log", required=True)
    p.add_argument("--agent", default="yfinance-quote")
    p.add_argument("--max-tokens", type=int, default=8_000_000)

    args = ap.parse_args()
    if args.cmd == "wait-done":
        return wait_done(args.db, args.sid, args.timeout)
    return verdict(args.db, args.sid, args.log, args.agent, args.max_tokens)


if __name__ == "__main__":
    sys.exit(main())
