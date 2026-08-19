#!/usr/bin/env python3
"""credential-register smoke: wait + verdict + leak scan.

Subcommands:
  wait-done   Block until the session tree's tool activity goes quiet AND no
              workflow tasks remain running/queued.
  verdict     Inspect the gateway store, the reply, the mock service log and
              every store artifact; decide PASS/FAIL/INCONCLUSIVE. The core
              assertion is the leak scan: the demo secret must appear ONLY
              in the encrypted vault — nowhere else, in any bytes.

Reads the gateway SQLite store read-only (stdlib sqlite3). Token accounting
is parsed from the gateway log's `llm exchange` lines.
"""

from __future__ import annotations

import argparse
import re
import sqlite3
import sys
import time
from collections import Counter
from pathlib import Path

POLL_SECS = 4.0
STABLE_POLLS = 8  # 32s of silence, plus no running/queued tasks

LLM_LINE = re.compile(
    r"llm exchange agent_id=(\S+) session_id=(\S+) model=(\S+) "
    r"input_tokens=(\d+) output_tokens=(\d+)"
)


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


def active_tasks(conn: sqlite3.Connection, sid: str) -> list:
    rows = subtree_rows(conn, "task_runs", sid, "task_id, agent_id, status")
    return [r for r in rows if r["status"] in ("running", "queued")]


def pending_approvals(conn: sqlite3.Connection, sid: str) -> list:
    """Open operator gates for the root-session subtree. A task suspended at
    an approval gate is not 'quiet' — the run is not done until every gate
    is resolved (approved/rejected/expired/cancelled)."""
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
    its own grievances sit unread is exactly the rubber-stamp pattern the
    RFCs warn about, so these surface as a FAIL-worthy section (#1108/3)."""
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


def scan_bytes(path: Path, needle: str) -> int:
    """Count occurrences of needle in a file's raw bytes (UTF-8 encoded).

    Streams in chunks with overlap so a multi-hundred-MB gateway.log or
    gateway.db(+wal) doesn't have to fit in memory.
    """
    chunk = 8 * 1024 * 1024
    needle_b = needle.encode("utf-8")
    overlap = len(needle_b) - 1
    count = 0
    tail = b""
    try:
        with open(path, "rb") as fh:
            while True:
                buf = fh.read(chunk)
                if not buf:
                    break
                count += (tail + buf).count(needle_b)
                tail = buf[-overlap:] if overlap > 0 else b""
    except FileNotFoundError:
        return 0
    return count


def leak_scan(run_dir: Path, secret: str) -> list[str]:
    """The invariant: the secret appears ONLY in the encrypted vault (which
    stores it wrapped, so the plaintext must not be found even there)."""
    findings = []
    targets = [
        # (label, path, allowed_count)
        ("gateway.log", run_dir / "gateway.log", 0),
        ("reply", run_dir / "reply.txt", 0),
        ("resolver.log", run_dir / "resolver.log", 0),
        ("mock.log", run_dir / "mock.log", 0),
        ("gateway.db", run_dir / "agents/.gateway/gateway.db", 0),
        ("gateway.db-wal", run_dir / "agents/.gateway/gateway.db-wal", 0),
        ("gateway.db-shm", run_dir / "agents/.gateway/gateway.db-shm", 0),
        ("vault.enc.json", run_dir / "agents/.gateway/vault.enc.json", 0),
    ]
    digest_root = run_dir / "agents/.gateway/sessions"
    if digest_root.is_dir():
        for p in sorted(digest_root.rglob("*")):
            if p.is_file():
                targets.append((f"digest:{p.relative_to(run_dir)}", p, 0))
    for label, path, allowed in targets:
        n = scan_bytes(path, secret)
        if n > allowed:
            findings.append(f"LEAK [{label}]: secret appears {n}x in {path}")
    return findings


def token_report(log_path: Path, sid: str):
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


def verdict(run_dir: Path, sid: str, secret: str, service: str,
            max_gates: int) -> int:
    db_path = run_dir / "agents/.gateway/gateway.db"
    reply_path = run_dir / "reply.txt"
    mock_log = run_dir / "mock.log"

    with connect(str(db_path)) as conn:
        try:
            creds = list(conn.execute(
                "SELECT credential_id, service, inject_as, allowed_hosts "
                "FROM credentials WHERE service = ?", (service,)))
        except sqlite3.OperationalError:
            creds = []
        try:
            # Scoped to the root-session subtree: `gateway.db` is global, and
            # this script must stay correct even pointed at a non-fresh store.
            approvals = list(conn.execute(
                "SELECT action_type, status FROM approvals "
                "WHERE root_session_id = ? OR root_session_id LIKE ? || '/%' "
                "OR session_id = ? OR session_id LIKE ? || '/%'",
                (sid, sid, sid, sid)))
        except sqlite3.OperationalError:
            approvals = []
        tasks = subtree_rows(conn, "task_runs", sid)
        traces = subtree_rows(conn, "execution_traces", sid, "trace_id")
        flags = unadjudicated_flags(conn, sid)

    reply = reply_path.read_text(encoding="utf-8", errors="replace") \
        if reply_path.exists() else ""
    mock = mock_log.read_text(encoding="utf-8", errors="replace") \
        if mock_log.exists() else ""
    # The final report can land in the chat reply OR only in the session
    # record: `chat --test-mode` is one-shot, and a root turn that suspends
    # (WaitingForChild) finishes as a notification-driven continuation with
    # no connected chat consumer — the yfinance demo has the same shape, so
    # the verdict reads the store like verdict.py there does.
    record = "\n".join(
        (t["result_summary"] or "") if "result_summary" in t.keys() else ""
        for t in tasks)
    report_surface = reply + "\n" + record

    cred = creds[0] if creds else None
    cred_ok = cred is not None
    inject_ok = cred is not None and "X-Api-Key" in (cred["inject_as"] or "")
    injection_ok = "200 ok" in mock and "city=toulouse" in mock
    weather_reported = ('"temperature_c"' in report_surface
                        or "temperature_c" in report_surface)
    cred_id_in_reply = bool(cred) and cred["credential_id"] in report_surface
    auth_rejections = mock.count("401")
    leaks = leak_scan(run_dir, secret)
    gates = Counter(
        f"{a['action_type']}/{a['status']}" for a in approvals)
    per_agent, tin, tout = token_report(run_dir / "gateway.log", sid)

    print("=" * 64)
    print("credential-register smoke verdict")
    print("=" * 64)
    print(f"workflow tasks:  {dict(Counter(t['status'] for t in tasks))}")
    print(f"tool traces:     {len(traces)}")
    print(f"credential:      "
          f"{cred['credential_id'] if cred else 'NONE'} "
          f"(inject_as={cred['inject_as'] if cred else '-'})")
    print(f"injection proof: {'mock saw a valid X-Api-Key 200' if injection_ok else 'no authenticated 200 in mock log'}")
    print(f"auth rejections: {auth_rejections} (401s seen by the mock)")
    print(f"weather reported:{'yes' if weather_reported else 'NO'}")
    print(f"credential_id stated: {'yes' if cred_id_in_reply else 'NO'}")
    print(f"approvals:       {dict(gates) or 'none'}")
    print(f"tokens (root tree): {tin + tout:,} ({tin:,} in, {tout:,} out)")
    for name, toks in per_agent.most_common(5):
        print(f"  {name:<32} {toks:>12,}")
    if flags:
        print("UNADJUDICATED FLAGS (complaints filed, nobody read them):")
        for f in flags:
            print(f"  {f['flag_id']} [{f['severity']}/{f['status']}] "
                  f"by {f['reporter_agent_id']}: {f['observation']}")
    else:
        print("unadjudicated anomaly flags: none")
    print()

    checks = [
        ("credential stored for service", cred_ok),
        ("inject_as is header:X-Api-Key", inject_ok),
        ("gateway-injected authenticated request (mock 200)", injection_ok),
        ("weather reported in final reply", weather_reported),
        ("credential_id stated in reply", cred_id_in_reply),
        ("no secret leak anywhere", not leaks),
        ("no un-adjudicated anomaly flags", not flags),
    ]

    print("-" * 64)
    print("VERDICT")
    print("-" * 64)
    for label, ok in checks:
        print(f"  [{'ok' if ok else 'FAIL'}] {label}")
    for f in leaks:
        print(f"  {f}")

    if not traces:
        print("INCONCLUSIVE: no tool activity recorded — the task never ran.")
        return 2
    if all(ok for _, ok in checks):
        gate_total = sum(v for k, v in gates.items() if k.endswith("/approved"))
        note = ("" if gate_total <= max_gates else
                f" — WARNING: {gate_total} approvals exceeds the expected "
                f"{max_gates} (study finding, see RFC §3.5 absurdity check)")
        print(f"PASS: credential lifecycle complete, secret never left the "
              f"vault.{note}")
        return 0
    print("FAIL: see the failed checks above.")
    return 1


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    sub = ap.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("wait-done")
    p.add_argument("--db", required=True)
    p.add_argument("--sid", required=True)
    p.add_argument("--timeout", type=int, default=1800)

    p = sub.add_parser("verdict")
    p.add_argument("--run-dir", required=True, type=Path)
    p.add_argument("--sid", required=True)
    p.add_argument("--secret", required=True)
    p.add_argument("--service", default="mockweather")
    p.add_argument("--max-gates", type=int, default=2)

    args = ap.parse_args()
    if args.cmd == "wait-done":
        return wait_done(args.db, args.sid, args.timeout)
    return verdict(args.run_dir, args.sid, args.secret, args.service,
                   args.max_gates)


if __name__ == "__main__":
    sys.exit(main())
