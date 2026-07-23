#!/usr/bin/env python3
"""Evolution digest: operator-facing summary of curation and evolution activity.

Queries the gateway SQLite store for stored patterns, curator decisions,
evolution flags, and promote_to_skill entries — renders a readable report.

Usage: python3 evolution_digest.py --db <gateway.db> [--hours 24] [--out report.md]
"""

from __future__ import annotations

import argparse
import datetime
import json
import sqlite3
import sys


def fmt_ts(ts: str | None) -> str:
    if not ts:
        return "?"
    try:
        dt = datetime.datetime.fromisoformat(ts.replace("Z", "+00:00"))
        return dt.strftime("%m-%d %H:%M")
    except ValueError:
        return ts[:16]


def hours_ago_iso(hours: int) -> str:
    return (datetime.datetime.now(datetime.timezone.utc)
            - datetime.timedelta(hours=hours)).isoformat()


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--db", required=True)
    ap.add_argument("--hours", type=int, default=168, help="Look back this many hours (default: 168 = 7 days)")
    ap.add_argument("--out", "-o", default=None)
    args = ap.parse_args()

    conn = sqlite3.connect(f"file:{args.db}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    since = hours_ago_iso(args.hours)

    lines: list[str] = []
    lines.append("# Evolution Digest\n")
    lines.append(f"*Covering last {args.hours} hours — generated {fmt_ts(None)}*\n")

    # ── Stored patterns ───────────────────────────────────────────────
    lines.append("## Patterns Stored\n")
    patterns = list(conn.execute(
        "SELECT created_at, content, confidence, tags FROM memories "
        "WHERE scope='evolution/patterns' AND created_at > ? ORDER BY created_at DESC",
        (since,)))
    if patterns:
        lines.append(f"**{len(patterns)}** pattern(s) stored in this window.\n")
        for p in patterns:
            tags = json.loads(p["tags"]) if p["tags"] else []
            ptype = next((t.replace("type:", "") for t in tags if t.startswith("type:")), "?")
            agents = [t.replace("agent:", "") for t in tags if t.startswith("agent:")]
            lines.append(f"- [{fmt_ts(p['created_at'])}] ({ptype}) conf={p['confidence']}")
            lines.append(f"  {p['content']}")
            if agents:
                lines.append(f"  *agents: {', '.join(agents)}*")
            lines.append("")
    else:
        lines.append("No patterns stored in this window.\n")

    # ── Pattern type distribution (noise check) ───────────────────────
    lines.append("## Pattern Distribution (dedup check)\n")
    # Group by type: tag only (not full tag string, which is unique per pattern)
    type_counts: dict[str, int] = {}
    all_patterns = list(conn.execute(
        "SELECT tags FROM memories WHERE scope='evolution/patterns'"))
    for p in all_patterns:
        tags = json.loads(p["tags"]) if p["tags"] else []
        ptype = next((t.replace("type:", "") for t in tags if t.startswith("type:")), "unknown")
        type_counts[ptype] = type_counts.get(ptype, 0) + 1
    for ptype, count in sorted(type_counts.items(), key=lambda x: -x[1]):
        lines.append(f"- **{ptype}**: {count} pattern(s)")
    total = len(all_patterns)
    lines.append(f"\n**Total patterns in store: {total}**\n")
    if total > 30:
        lines.append("⚠️  High pattern count — consider dedup/merge pass.\n")

    # ── Curator decisions ─────────────────────────────────────────────
    lines.append("## Curator Decisions\n")
    decisions = list(conn.execute(
        "SELECT timestamp, session_id, target, payload FROM causal_events "
        "WHERE category='curator' AND action='decision' AND timestamp > ? ORDER BY timestamp DESC LIMIT 20",
        (since,)))
    if decisions:
        lines.append(f"**{len(decisions)}** decision(s) in this window.\n")
        for d in decisions:
            payload = json.loads(d["payload"]) if d["payload"] else {}
            action = payload.get("action", "?")
            conf = payload.get("confidence", "?")
            target = d["target"] or payload.get("target", "?")
            action_emoji = {"promote_to_skill": "🏆", "flag_for_evolution": "🚩", "keep": "✅", "drop": "🗑️"}.get(action, "?")
            lines.append(f"- [{fmt_ts(d['timestamp'])}] {action_emoji} **{action}** `{target}` (conf={conf})")
            reason = payload.get("reason_detail", "")
            if reason:
                lines.append(f"  > {reason[:200]}")
            if payload.get("proposed_instruction"):
                lines.append(f"  **Proposed:** `{payload['proposed_instruction']}`")
            lines.append("")
    else:
        lines.append("No curator decisions in this window.\n")

    # ── Evolution flags ───────────────────────────────────────────────
    lines.append("## Evolution Flags\n")
    flags = [d for d in decisions
             if (json.loads(d["payload"]) if d["payload"] else {}).get("action") == "flag_for_evolution"]
    if flags:
        for f in flags:
            payload = json.loads(f["payload"]) if f["payload"] else {}
            target = f["target"] or payload.get("target", "?")
            lines.append(f"- 🚩 **`{target}`** flagged for evolution")
            reason = payload.get("reason_detail", "")
            if reason:
                lines.append(f"  {reason[:250]}")
            lines.append("")
    else:
        lines.append("No agents flagged for evolution.\n")

    # ── Promote to skill ──────────────────────────────────────────────
    lines.append("## Skill Graduations (promote_to_skill)\n")
    promotions = [d for d in decisions if
                  (json.loads(d["payload"]) if d["payload"] else {}).get("action") == "promote_to_skill"]
    if promotions:
        # Deduplicate identical promote_to_skill entries (same target+agent+instruction)
        seen: set[str] = set()
        for p in promotions:
            payload = json.loads(p["payload"]) if p["payload"] else {}
            target = p["target"] or payload.get("target", "?")
            agent = payload.get("target_agent") or "(not specified)"
            instruction = payload.get("proposed_instruction") or "(not specified)"
            key = f"{target}|{agent}|{instruction}"
            if key in seen:
                continue
            seen.add(key)
            lines.append(f"- 🏆 **`{target}`** → **`{agent}`**")
            lines.append(f"  **Instruction:** `{instruction}`")
            lines.append("")
    else:
        lines.append("No lessons promoted to SKILL.md.\n")

    # ── Skipped graduations (from evolution/graduations with graduation_skipped tags) ──
    skipped = list(conn.execute(
        "SELECT content, tags, created_at FROM memories "
        "WHERE scope='evolution/graduations' AND tags LIKE '%type:graduation_skipped%' "
        "AND created_at > ? ORDER BY created_at DESC", (since,)))
    if skipped:
        lines.append("## Skipped Graduations (exempt or blocked)\n")
        for s in skipped:
            try:
                c = json.loads(s["content"])
            except json.JSONDecodeError:
                c = {"raw": s["content"]}
            agent = c.get("target_agent", "?")
            reason = c.get("skip_reason", c.get("reason", "unknown"))
            instruction = c.get("proposed_instruction", "")
            lines.append(f"- ⏭️ **`{agent}`** — skipped: {reason}")
            if instruction:
                lines.append(f"  **Proposed instruction:** `{instruction}`")
            lines.append("")
    else:
        lines.append("No graduations skipped in this window.\n")

    # ── Agent revisions (evolution changelog) ─────────────────────────
    lines.append("## Agent Revisions\n")
    try:
        revs = list(conn.execute(
            "SELECT agent_id, revision_id, created_at, created_by_id FROM agent_revisions "
            "WHERE created_at > ? ORDER BY created_at DESC LIMIT 10", (since,)))
        if revs:
            for r in revs:
                lines.append(f"- [{fmt_ts(r['created_at'])}] `{r['agent_id']}` — `{r['revision_id'][:16]}…` by {r['created_by_id']}")
                lines.append("")
        else:
            lines.append("No agent revisions in this window.\n")
    except sqlite3.OperationalError as e:
        lines.append(f"Agent revisions query failed: {e}\n")

    report = "\n".join(lines)
    if args.out:
        with open(args.out, "w") as f:
            f.write(report)
        print(f"Report written to {args.out}", file=sys.stderr)
    else:
        print(report)


if __name__ == "__main__":
    main()
