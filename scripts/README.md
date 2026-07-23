# Operator Scripts

Human-facing tools for inspecting the gateway's evolution pipeline — curation
history, stored patterns, evolution decisions, and skill graduations. All
scripts query the gateway SQLite store directly (read-only) and require only
Python 3 stdlib.

## Quick start

```bash
# Point at your gateway store (default: <agents_dir>/.gateway/gateway.db)
DB=path/to/agents/.gateway/gateway.db

# Evolution digest: what patterns were stored, which agents were flagged,
# what was promoted to SKILL.md, dedup pressure warnings
python3 scripts/evolution_digest.py --db $DB --hours 24

# Recap all curator sessions: which succeeded, which failed and why
python3 scripts/recap_curations.py --db $DB

# Render a single curator run's JSON output as readable Markdown
cat curator_output.json | python3 scripts/render_curation.py
```

## Scripts

| Script | Purpose |
|---|---|
| `evolution_digest.py` | Operator digest: patterns stored, decisions, evolution flags, graduations, agent revisions |
| `recap_curations.py` | Timeline of all curator sessions with outcomes and LoopGuard causes |
| `render_curation.py` | Per-run formatter: curator JSON output → readable Markdown |

## Demo

See `smoke/memory-loop/` for the end-to-end demo that exercises the full
digest → curation → evolution loop with the weathermerge trap project.
