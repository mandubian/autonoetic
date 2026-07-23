# memory-loop demo — watch the gateway learn from one session and use it in the next

This demo makes the auto-learning pipeline visible end to end:

1. **Cold run** — `planner.default` is given a build task against a fresh
   gateway store (no memories). The task contains a deliberate trap, so the
   agent has to fail before it can succeed.
2. **Post-session digest** — when the session closes, the gateway-internal
   `autonoetic.digest` agent reads the live digest + execution traces and
   extracts durable lessons into Tier-2 memories
   (`source:post_session_digest`, global visibility, agent/session tags).
3. **Warm run** — a brand-new root session gets the *identical* task text.
   At wake time the gateway builds a "Prior knowledge" block from digest
   memories, ranked by token overlap with the task
   (`runtime/context.rs::build_memory_context_snippet`, with error lessons
   prioritized on ties). The agent walks around the trap instead of tripping it.
4. **Report** — `report.py` compares both runs from the gateway SQLite store:
   tool-call failures, `EMANIFEST` hits, wall time — and prints the exact
   memories that were available to the warm run.

Because the warm session is a *new root session* with the *same task text*,
the only causal difference between the two runs is the primed memory.

## The trap: `trap-project/` (weathermerge)

A tiny stdlib-only Python project. Its README tells you to run
`python3 main.py` — which fails cryptically:

```
weathermerge: error: EMANIFEST (resource sealed) — aborting   (exit 3)
```

`main.py` requires `build/manifest.json`, a sealed manifest of data-file
digests that does not exist in a fresh checkout. The fix is a build step:
`python3 tools/seal.py`, or simply `make` (whose default target chains
`seal → report`; the bare `report` target is the trap). Neither the error
message nor the README says this — the agent discovers it by inspecting the
Makefile / `tools/`, exactly the kind of lesson a digest should capture.

Try it yourself:

```sh
cd trap-project && python3 main.py   # EMANIFEST, exit 3
make                                  # works
make clean
```

## Running

```sh
cargo build -p autonoetic
export OPENROUTER_API_KEY=...          # or override the model, see below
smoke/memory-loop/run_demo.sh
```

The script is fully self-contained: it creates a throwaway gateway home
under `.run/`, bootstraps the reference agents into it, starts a gateway
daemon, runs an approval/interaction auto-resolver in the background (the
"operator" is a script — every approval is approved at root scope), drives
both sessions through `chat --test-mode`, waits for the digest, and prints
the comparison report. Artifacts (config, session replies, report,
`gateway.db`) stay in `.run/` for inspection; each run wipes and recreates
`.run/`, so the cold run always starts with an empty memory store.

Knobs (environment variables):

- `MEMORY_LOOP_MODEL` — LLM for all presets (default `minimax/minimax-m2.7`
  via the `openrouter` provider). All four presets used here (`smart`,
  `coding`, `research`, `agentic`) are pointed at this one model.
- `MEMORY_LOOP_PORT` — gateway JSON-RPC port (default `4177`; OFP port is
  `+100`). The HTTP ingress is disabled (`http_port: 0`) so the demo never
  clashes with a concurrently running gateway.
- `AUTONOETIC_BIN` — path to the `autonoetic` binary (default
  `target/debug/autonoetic`).

## What a successful run looks like

Actual output from a live run (minimax-m2.7 via OpenRouter, 2026-07-22):

```
COLD run (root session: ml-cold) — empty memory store
  tool calls:      34 (failed: 3)
  failures by tool: resolve×2, sandbox_exec×1
  EMANIFEST hits:  0
  agent sessions:  7
  wall time:       2m47s

Digest memories extracted from the cold run (3):
  [digest.error_pattern] (agent:planner.default conf=0.90)
    Direct `make` invocations fail with permission error P-1.9 in
    sandbox_exec; wrapping commands in `bash -c '...'` bypasses this
    CodeExecution pattern restriction
  [digest.fact] (agent:planner.default conf=0.95)
    The weathermerge project ... requires a `make` (or `make seal report`)
    build step that runs tools/seal.py before main.py

WARM run (root session: ml-warm) — primed with the above
  tool calls:      18 (failed: 2)
  EMANIFEST hits:  0
  agent sessions:  2
  wall time:       1m26s

SUCCESS: the warm run avoided failures the cold run had to make.
```

Two honest caveats from that run:

- **EMANIFEST was never tripped — in either run.** The model inspected the
  README/Makefile *before* running `main.py`, so the designed trap never
  sprung. The lesson that actually crystallized was the seal-before-`main.py`
  build order (plus a sandbox policy quirk: bare `make` is rejected by the
  CodeExecution pattern gate, `bash -c 'make'` is not). The warm executor
  recovered from the `make` rejection by going **straight to
  `python3 tools/seal.py && python3 main.py`** — exactly the primed fact —
  instead of the cold run's exploratory detour.
- **Digest memories are tagged per-agent** (`agent:planner.default` above),
  and priming is per-agent too: the warm *executor* did not inherit the
  planner's `make`-policy lesson and re-tripped it once. Lessons reach
  sibling agents only indirectly (e.g. via the planner's spawn message) —
  visible in the traces if you look for it.

The strongest priming evidence is in the warm run's own trace: the planner
explicitly called `digest_query`/`session_search` for prior build context,
and its reply referenced "prior session knowledge" about weathermerge — in a
fresh root session, that knowledge could only have come from the digest.

## Reading more out of the run

Everything is in `.run/agents/.gateway/gateway.db`:

```sql
-- the memories the digest stored
SELECT scope, content, tags FROM memories
WHERE tags LIKE '%source:post_session_digest%';

-- every tool call of a run, in order
SELECT timestamp, agent_id, tool_name, success, error_summary
FROM execution_traces
WHERE session_id = 'ml-cold' OR session_id LIKE 'ml-cold/%'
ORDER BY timestamp;
```

## Caveats and knobs that matter

- **Token-overlap recall.** Priming ranks memories by Jaccard overlap with
  the task text. The task text and the trap share tokens (`weathermerge`,
  `main.py`, `report.md`, `data`) on purpose. If you change the task wording,
  keep that vocabulary or the lesson may not surface.
- **`digest_agent.llm_preset` is required.** Without it the digest is skipped
  with an error log (`resolve_digest_llm_config`). The generated config sets
  it to `agentic`.
- **Priming limit.** Default profile is `standard` → `memory_priming_limit`
  = 5. This demo stores fewer memories than that, so everything fits.
- **LLM variance.** The warm run can still trip the trap if the model ignores
  the primed block — the report verdict distinguishes "memory had measurable
  effect" from "inconclusive". Re-running is cheap; the store is reset each
  time.

## Where this goes next

This demo shows the first two rungs of the ladder (digest → recall). The
third rung is graduation: after the same lesson recurs across ≥3 sessions and
≥2 agents, `memory-curator.default` (evolution pipeline) can emit a
`promote_to_skill` decision so the lesson crystallizes into a SKILL.md
instruction — at which point even a run with memory priming disabled avoids
the trap. See `agents/evolution/memory-curator.default/SKILL.md`.
