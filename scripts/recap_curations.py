#!/usr/bin/env python3
"""Recap: show all curator sessions and their stored patterns from the gateway DB.

Usage: python3 recap_curations.py --db <gateway.db>
"""

from __future__ import annotations

import argparse
import datetime
import sqlite3
import sys


SESSION_COLORS = {
    "curator-run-1": "❌",
    "curator-run-2": "❌",
    "curator-run-3": "❌",
    "curator-run-4": "❌",
    "curator-run-5": "❌",
    "curator-run-6": "✅",
    "curator-run-7": "✅",
    "curator-run-8": "✅",
    "curator-promote": "❌",
}

SID_PREFIX = "curator"


def fmt_ts(ts: str | None) -> str:
    if not ts:
        return "?"
    try:
        dt = datetime.datetime.fromisoformat(ts.replace("Z", "+00:00"))
        return dt.strftime("%m-%d %H:%M")
    except ValueError:
        return ts[:16]


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--db", required=True)
    args = ap.parse_args()

    conn = sqlite3.connect(f"file:{args.db}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row

    # Find all curator sessions
    sids = sorted(set(
        r["session_id"] for r in conn.execute(
            "SELECT DISTINCT session_id FROM execution_traces WHERE session_id LIKE ?", (f"{SID_PREFIX}%",)
        )
    ))

    if not sids:
        print("No curator sessions found.")
        return

    for sid in sids:
        outcome = SESSION_COLORS.get(sid, "?")
        print(f"\n{'=' * 64}")
        print(f"{outcome}  {sid}")

        # Session timing and status
        r = conn.execute(
            "SELECT started_at, ended_at, status FROM session_transcripts WHERE session_id=?", (sid,)
        ).fetchone()
        if r:
            print(f"  when: {fmt_ts(r['started_at'])} → {fmt_ts(r['ended_at'])}  status: {r['status']}")

        # Traces
        trace_count = conn.execute(
            "SELECT COUNT(*) FROM execution_traces WHERE session_id=?", (sid,)
        ).fetchone()[0]
        ok_count = conn.execute(
            "SELECT COUNT(*) FROM execution_traces WHERE session_id=? AND success=1", (sid,)
        ).fetchone()[0]
        print(f"  calls: {trace_count} total ({ok_count} ok, {trace_count - ok_count} failed)")

        # Knowledge_store calls (what was actually persisted)
        stored = list(conn.execute(
            "SELECT substr(coalesce(result,''),1,200) AS r FROM execution_traces "
            "WHERE session_id=? AND tool_name='knowledge_store'", (sid,)
        ))
        if stored:
            print(f"  patterns stored: {len(stored)}")
            for s in stored:
                r = s["r"]
                # Extract the content from the result JSON if possible
                if r and '"ok":true' in r:
                    lines = r.split("\\n") if "\\n" in r else r.split("\n")
                    for line in lines[:3]:
                        print(f"    > {line.strip()[:100]}")
        else:
            print(f"  patterns stored: 0 (session did not reach Store phase)")

        # Failed tools (top 3)
        fail_tools = list(conn.execute(
            "SELECT tool_name, COUNT(*) AS n FROM execution_traces "
            "WHERE session_id=? AND success=0 GROUP BY tool_name ORDER BY n DESC LIMIT 3", (sid,)
        ))
        if fail_tools:
            fails = ", ".join(f"{r['tool_name']}×{r['n']}" for r in fail_tools)
            print(f"  main failures: {fails}")

        # LoopGuard if any
        lg = conn.execute(
            "SELECT payload FROM causal_events WHERE session_id=? AND category='loop_guard'", (sid,)
        ).fetchone()
        if lg:
            print(f"  ⛔ LoopGuard: {lg['payload'][:120]}")

    # Summary: all stored patterns (across all curations)
    print(f"\n\n{'=' * 64}")
    print("ALL STORED PATTERNS (evolution/patterns)")
    print(f"{'=' * 64}")
    for r in conn.execute(
        "SELECT created_at, scope, substr(content,1,100) AS content, confidence, tags FROM memories "
        "WHERE scope='evolution/patterns' ORDER BY created_at"
    ):
        tags = json.loads(r["tags"]) if r["tags"] and r["tags"].startswith("[") else []
        agent_tags = [t.replace("agent:", "") for t in tags if t.startswith("agent:")]
        type_tag = next((t.replace("type:", "") for t in tags if t.startswith("type:")), "?")
        print(f"  [{fmt_ts(r['created_at'])}] ({type_tag}) conf={r['confidence']}")
        print(f"    {r['content']}")
        if agent_tags:
            print(f"    agents: {', '.join(agent_tags)}")


if __name__ == "__main__":
    import json
    sys.exit(main())
