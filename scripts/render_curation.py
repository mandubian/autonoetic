#!/usr/bin/env python3
"""Render the memory curator's JSON output as a readable Markdown report.

Usage: cat curator_output.json | python3 render_curation.py [--out report.md]
       report.py render --db <gateway.db> --sid <curator-session-id>
"""

from __future__ import annotations

import argparse
import json
import sqlite3
import sys


def load_json(path_or_stdin: str | None):
    if path_or_stdin:
        with open(path_or_stdin) as f:
            return json.load(f)
    return json.load(sys.stdin)


def render(data: dict) -> str:
    lines: list[str] = []
    lines.append("# Memory Curation Report\n")

    # ── Learnings stored ──────────────────────────────────────────────
    ls = data.get("learnings_stored", 0)
    lines.append(f"**{ls}** new learning(s) stored in `evolution/patterns`.\n")

    # ── Agent scores ──────────────────────────────────────────────────
    scores = data.get("agent_scores", {})
    if scores:
        lines.append("## Agent Scores\n")
        lines.append("| Agent | Failure Rate | Repeated Errors | Approval Denials | Eval Score | Signals | Evolution Rec. |")
        lines.append("|---|---|---|---|---|---|---|")
        for agent_id, s in sorted(scores.items()):
            evo = "⚠️  Yes" if s.get("evolution_recommended") else "No"
            errors = ", ".join(s.get("repeated_errors", [])) or "—"
            lines.append(
                f"| `{agent_id}` | {s.get('failure_rate', '?')} | {errors} | "
                f"{s.get('approval_denial_count', 0)} | {s.get('eval_score', '?')} | "
                f"{s.get('signals_triggered', 0)} | {evo} |"
            )
        lines.append("")
        for agent_id, s in sorted(scores.items()):
            summary = s.get("evidence_summary", "")
            if summary:
                lines.append(f"<details><summary><code>{agent_id}</code> — evidence</summary>\n\n{summary}\n\n</details>\n")

    # ── Decision journal ──────────────────────────────────────────────
    djournal = data.get("decision_journal", [])
    if djournal:
        lines.append("## Decision Journal\n")
        by_action: dict[str, list] = {}
        for entry in djournal:
            action = entry.get("action", "?")
            by_action.setdefault(action, []).append(entry)

        for action in ["promote_to_skill", "flag_for_evolution", "keep", "drop"]:
            entries = by_action.pop(action, [])
            if not entries:
                continue
            action_label = {"promote_to_skill": "🏆  Promote to Skill",
                            "flag_for_evolution": "🚩  Flag for Evolution",
                            "keep": "✅  Keep",
                            "drop": "🗑️  Drop"}.get(action, action)
            lines.append(f"### {action_label} ({len(entries)})\n")
            for e in entries:
                target = e.get("target", "?")
                reason = e.get("reason_detail", e.get("reason_code", ""))
                conf = e.get("confidence", "?")
                lines.append(f"- **`{target}`** — conf={conf}")
                if reason:
                    # Truncate long details
                    if len(reason) > 400:
                        reason = reason[:397] + "..."
                    lines.append(f"  > {reason}")
                if e.get("proposed_instruction"):
                    lines.append(f"  **Proposed instruction:** `{e['proposed_instruction']}`")
                lines.append("")
        if by_action:
            for action, entries in by_action.items():
                lines.append(f"### {action} ({len(entries)})\n")
                for e in entries:
                    lines.append(f"- **`{e.get('target', '?')}`** — conf={e.get('confidence', '?')}")
                    lines.append("")

    # ── Systemic gaps ─────────────────────────────────────────────────
    gaps = data.get("systemic_gaps", [])
    if gaps:
        lines.append("## Systemic Gaps\n")
        for i, g in enumerate(gaps, 1):
            title = g.get("title", "?")
            priority = g.get("priority", "?")
            blast = g.get("blast_radius", "?")
            lines.append(f"### {i}. {title}")
            lines.append(f"**Priority:** {priority} | **Blast radius:** {blast}\n")
            evidence = g.get("evidence", {})
            if isinstance(evidence, dict):
                for k, v in evidence.items():
                    lines.append(f"- **{k}:** {v}")
            lines.append("")
            remediation = g.get("remediation", "")
            if remediation:
                lines.append(f"**Remediation:** {remediation}\n")

    # ── Anomalies ─────────────────────────────────────────────────────
    anomalies = data.get("anomalies", [])
    if anomalies:
        lines.append("## Anomalies\n")
        for a in anomalies:
            obs = a.get("observation", "")
            sev = a.get("severity", "?")
            ref = a.get("subject_ref", "")
            lines.append(f"- [{sev}] {obs}")
            if ref:
                lines.append(f"  *ref: {ref}*")
            lines.append("")

    return "\n".join(lines)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("input", nargs="?", help="JSON file (omit for stdin)")
    ap.add_argument("--out", "-o", default=None, help="Write to file instead of stdout")
    ap.add_argument("--db", help="Read from gateway.db + session id")
    ap.add_argument("--sid", help="Session id for --db mode")
    args = ap.parse_args()

    if args.db and args.sid:
        conn = sqlite3.connect(f"file:{args.db}?mode=ro", uri=True)
        rows = list(conn.execute(
            "SELECT payload FROM causal_events "
            "WHERE session_id=? AND action='io.returns' AND category='contract' "
            "ORDER BY timestamp DESC LIMIT 1", (args.sid,)))
        if not rows:
            print("No io.returns event found for that session.", file=sys.stderr)
            sys.exit(1)
        data = json.loads(rows[0][0])
    else:
        data = load_json(args.input)

    report = render(data)
    if args.out:
        with open(args.out, "w") as f:
            f.write(report)
        print(f"Report written to {args.out}", file=sys.stderr)
    else:
        print(report)


if __name__ == "__main__":
    main()
