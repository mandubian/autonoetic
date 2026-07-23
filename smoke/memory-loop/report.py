#!/usr/bin/env python3
"""memory-loop demo: session waits and cold/warm comparison report.

Subcommands:
  wait-done     Block until a root session's tool activity goes quiet.
  wait-digest   Block until post-session digest memories exist for a session.
  compare       Print the cold-vs-warm comparison report.

All state is read directly from the gateway SQLite store
(<agents_dir>/.gateway/gateway.db) with the stdlib sqlite3 module.
"""

from __future__ import annotations

import argparse
import sqlite3
import sys
import time

POLL_SECS = 4.0
STABLE_POLLS = 4  # consecutive polls with no new traces => "quiet"


def connect(db_path: str) -> sqlite3.Connection:
    conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True, timeout=10)
    conn.row_factory = sqlite3.Row
    return conn


def trace_count(conn: sqlite3.Connection, sid: str) -> int:
    row = conn.execute(
        "SELECT COUNT(*) AS n FROM execution_traces "
        "WHERE session_id = ? OR session_id LIKE ? || '/%'",
        (sid, sid),
    ).fetchone()
    return int(row["n"])


def digest_memories(conn: sqlite3.Connection, sid: str | None) -> list[sqlite3.Row]:
    sql = (
        "SELECT scope, content, tags, confidence, created_at FROM memories "
        "WHERE tags LIKE '%source:post_session_digest%'"
    )
    params: tuple = ()
    if sid is not None:
        sql += " AND tags LIKE ?"
        params = (f'%session:{sid}"%',)
        # tags is a JSON array; the session tag is stored as "session:<base>"
    sql += " ORDER BY created_at"
    try:
        return list(conn.execute(sql, params))
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
        except sqlite3.Error:
            n = last  # transient lock; keep previous reading
        if n > 0 and n == last:
            stable += 1
        else:
            stable = 0
        last = n
        if stable >= STABLE_POLLS:
            print(f"[wait-done] session {sid}: {n} traces, quiet for "
                  f"{STABLE_POLLS} polls — done", flush=True)
            return 0
        print(f"[wait-done] session {sid}: traces={n} (waiting)", flush=True)
        time.sleep(POLL_SECS)
    print(f"[wait-done] TIMEOUT waiting for session {sid}", flush=True)
    return 1


def wait_digest(db_path: str, sid: str, timeout: int) -> int:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with connect(db_path) as conn:
                mems = digest_memories(conn, sid)
        except sqlite3.Error:
            mems = []
        if mems:
            print(f"[wait-digest] session {sid}: {len(mems)} digest memories",
                  flush=True)
            return 0
        print(f"[wait-digest] session {sid}: no digest memories yet", flush=True)
        time.sleep(POLL_SECS)
    print(f"[wait-digest] TIMEOUT waiting for digest of {sid}", flush=True)
    return 1


def session_stats(conn: sqlite3.Connection, sid: str) -> dict:
    rows = list(conn.execute(
        "SELECT tool_name, success, stderr, error_summary, timestamp, session_id "
        "FROM execution_traces WHERE session_id = ? OR session_id LIKE ? || '/%'",
        (sid, sid),
    ))
    failures = [r for r in rows if r["success"] == 0]
    emanifest = [
        r for r in rows
        if "EMANIFEST" in (r["stderr"] or "") or "EMANIFEST" in (r["error_summary"] or "")
    ]
    timestamps = sorted(r["timestamp"] for r in rows if r["timestamp"])
    wall_secs = None
    if len(timestamps) >= 2:
        try:
            from datetime import datetime
            t0 = datetime.fromisoformat(timestamps[0].replace("Z", "+00:00"))
            t1 = datetime.fromisoformat(timestamps[-1].replace("Z", "+00:00"))
            wall_secs = (t1 - t0).total_seconds()
        except ValueError:
            wall_secs = None
    fail_by_tool: dict[str, int] = {}
    for r in failures:
        fail_by_tool[r["tool_name"]] = fail_by_tool.get(r["tool_name"], 0) + 1
    return {
        "tool_calls": len(rows),
        "failures": len(failures),
        "fail_by_tool": fail_by_tool,
        "emanifest": len(emanifest),
        "child_sessions": len({r["session_id"] for r in rows}),
        "wall_secs": wall_secs,
    }


def fmt_wall(secs: float | None) -> str:
    if secs is None:
        return "n/a"
    m, s = divmod(int(secs), 60)
    return f"{m}m{s:02d}s"


def print_stats(label: str, stats: dict) -> None:
    print(f"{label}")
    print(f"  tool calls:      {stats['tool_calls']} (failed: {stats['failures']})")
    if stats["fail_by_tool"]:
        breakdown = ", ".join(
            f"{tool}×{n}" for tool, n in sorted(stats["fail_by_tool"].items())
        )
        print(f"  failures by tool: {breakdown}")
    print(f"  EMANIFEST hits:  {stats['emanifest']}")
    print(f"  agent sessions:  {stats['child_sessions']}")
    print(f"  wall time:       {fmt_wall(stats['wall_secs'])}")


def compare(db_path: str, cold: str, warm: str) -> int:
    with connect(db_path) as conn:
        cold_stats = session_stats(conn, cold)
        warm_stats = session_stats(conn, warm)
        # Memories available when the warm session started = digest memories
        # extracted from the cold session subtree.
        cold_mems = digest_memories(conn, cold)
        warm_mems = digest_memories(conn, warm)

    print("=" * 64)
    print("weathermerge memory-loop report")
    print("=" * 64)
    print()
    print_stats(f"COLD run (root session: {cold}) — empty memory store", cold_stats)
    print()
    print(f"Digest memories extracted from the cold run ({len(cold_mems)}):")
    for m in cold_mems:
        agent_tag = next(
            (t for t in (m["tags"] or "").split('"') if t.startswith("agent:")),
            "agent:?",
        )
        conf = f" conf={m['confidence']:.2f}" if m["confidence"] is not None else ""
        print(f"  [{m['scope']}] ({agent_tag}{conf})")
        print(f"    {m['content']}")
    if not cold_mems:
        print("  (none — the digest did not run or produced no memories;")
        print("   the warm run had nothing to learn from)")
    print()
    print_stats(f"WARM run (root session: {warm}) — primed with the above", warm_stats)
    print()

    delta = cold_stats["failures"] - warm_stats["failures"]
    print("-" * 64)
    print("VERDICT")
    print("-" * 64)
    if not cold_mems:
        print("INCONCLUSIVE: no digest memories were stored after the cold run,")
        print("so memory priming could not influence the warm run.")
        return 2
    if warm_stats["tool_calls"] == 0:
        print("INCONCLUSIVE: the warm run produced no tool traces at all.")
        return 2
    print(f"failures:      {cold_stats['failures']} (cold) -> "
          f"{warm_stats['failures']} (warm)   [delta: {delta:+d}]")
    print(f"EMANIFEST hits: {cold_stats['emanifest']} (cold) -> "
          f"{warm_stats['emanifest']} (warm)")
    print(f"tool calls:    {cold_stats['tool_calls']} (cold) -> "
          f"{warm_stats['tool_calls']} (warm)")
    if delta > 0 or warm_stats["emanifest"] < cold_stats["emanifest"]:
        print()
        print("SUCCESS: the warm run avoided failures the cold run had to make.")
        print("The digested lesson demonstrably influenced the new session.")
        return 0
    print()
    print("NO MEASURABLE EFFECT: the warm run did not do better on these metrics.")
    print("Check that the digest memories above mention the trap (EMANIFEST /")
    print("seal / manifest) — priming ranks by token overlap with the task text.")
    return 1


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    sub = ap.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("wait-done")
    p.add_argument("--db", required=True)
    p.add_argument("--sid", required=True)
    p.add_argument("--timeout", type=int, default=900)

    p = sub.add_parser("wait-digest")
    p.add_argument("--db", required=True)
    p.add_argument("--sid", required=True)
    p.add_argument("--timeout", type=int, default=300)

    p = sub.add_parser("compare")
    p.add_argument("--db", required=True)
    p.add_argument("--cold", required=True)
    p.add_argument("--warm", required=True)

    args = ap.parse_args()
    if args.cmd == "wait-done":
        return wait_done(args.db, args.sid, args.timeout)
    if args.cmd == "wait-digest":
        return wait_digest(args.db, args.sid, args.timeout)
    return compare(args.db, args.cold, args.warm)


if __name__ == "__main__":
    sys.exit(main())
