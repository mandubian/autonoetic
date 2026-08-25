# Divergence Sentinel — Validation Experiment Protocol (P4)

> Status: **Protocol ready (2026-05-20)** — harness implemented, awaiting
> operator-curated corpus run.
> Tracking: [#243](https://github.com/mandubian/autonoetic/issues/243).
> Sister design: [`divergence-sentinel-design.md`](./divergence-sentinel.md) §6.

## 1. Purpose

The Sentinel design (`docs/proposals/divergence-sentinel.md`) proposes
two layers of divergence detection:

- **Layer 1** — deterministic, in-gateway trajectory monitor (shipped in
  P1 / #240).
- **Layer 2** — optional LLM watchdog agent (shipped in P3 / #242,
  manual-trigger only).

Before we wire Layer 2 to **auto-invoke** from Layer 1 escalations, we
need to falsify the operator's anecdotal observation that the watchdog
catches divergence Layer 1 misses. If the data does not show a clear
advantage, P3 stays manual-only.

This document specifies the experiment that produces the GO / NO-GO
decision.

## 2. Method

### 2.1 Corpus

The operator hand-selects **N archived sessions** (target: N = 20).

- **10 "diverged" sessions** — operator-flagged or ended in
  `LoopGuard` trip / emergency stop. The pool from which to draw these
  is the gateway store's `causal_events` and `execution_traces` tables
  for sessions tagged with `emergency_stop`, `loop_guard_tripped`, or
  manually annotated by the operator as "this should have been caught".
- **10 "succeeded" sessions** — terminated cleanly, no loop trip, no
  emergency stop, operator-accepted outcome. Pick sessions of similar
  duration / agent-id to the diverged set so the comparison is not
  trivially confounded by length.

A balanced corpus (10/10) keeps TPR and FPR readable. A different ratio
is fine but flag it in the report.

### 2.2 Corpus format

The corpus is a YAML file. Schema:

```yaml
sessions:
  - session_id: "abc12345-6789-..."
    label: "diverged"        # or "succeeded"
    notes: "looped on web.fetch 8 turns, operator killed at turn 12"
  - session_id: "def67890-..."
    label: "succeeded"
    notes: "completed in 6 turns, all passes"
  # ...
```

Accepted label synonyms (lowercase, trimmed):

- `diverged` / `divergent` / `failed` / `fail` → ground truth = positive
- `succeeded` / `success` / `healthy` / `ok` → ground truth = negative

Optional fields:

- `notes` — free-text, shown in the report.
- `cached_watchdog_reply` — used when re-running the harness with
  `--skip-watchdog` to avoid re-spending LLM tokens.

### 2.3 Running the harness

Default (tool-using watchdog):

```bash
autonoetic sentinel-experiment \
  --corpus path/to/sentinel-validation-corpus.yaml \
  --output path/to/results.md
```

**Recommended for complex sessions** — tool-free mode:

```bash
autonoetic sentinel-experiment \
  --corpus path/to/sentinel-validation-corpus.yaml \
  --output path/to/results.md \
  --no-tools
```

What the harness does, per session:

1. **Layer 1 verdict** — queries `causal_events` for the session's
   `divergence.*` events and computes the **highest level reached**
   (`watching` / `diverging` / `critical`). The session is flagged by
   Layer 1 iff it reached `diverging` or `critical`. (`watching` is
   observational only.)
2. **Watchdog verdict** — one of two modes:
   - **Default mode** runs `watchdog.default` with tools (`digest_query`,
     `execution_search`, `agent_message`, `session_escalate`). Multi-round
     LLM completion. Higher cost; can drill into novel evidence. Writes
     real notification rows on the target session (see §2.4).
   - **`--no-tools` mode** runs `watchdog-fast.default` **with an empty
     `NativeToolRegistry`**: no tools are declared to the LLM at all.
     (The manifest's empty `capabilities` list alone is not enough —
     several native tools such as `execution_search`,
     `session_escalate`, and `digest_annotate` are always-available
     regardless of capability gating. The empty registry is the
     load-bearing isolation.) The harness pre-renders a structured
     `SessionOverview` (tool histogram, recent errors, Layer 1
     snapshot, digest tail) into the kickoff message and accepts a
     single LLM completion as the verdict. Cheaper by roughly an order
     of magnitude on complex sessions; deterministic at
     `temperature=0.0`; writes no side-effect rows on the target
     session because there are no callable tools.
3. **Watchdog classification** — two-tier:
   - **Tier 1 (structured)**: if the reply begins with `VERDICT:
     diverging` or `VERDICT: critical`, the session is flagged.
     `VERDICT: healthy` / `VERDICT: watching` is not flagged. This is
     the path taken in `--no-tools` mode (the fast watchdog is
     instructed to emit the verdict on line 1).
   - **Tier 2 (keyword fallback)**: free-form replies are scanned for
     `diverging`, `divergence`, `critical`, `watching`, `loop pressure`,
     `failure pressure`, `repetition`, `stalled`, `escalat`.

### 2.3.1 Choosing between the modes

- Use `--no-tools` (`watchdog-fast.default`) when:
  - The corpus contains complex / long sessions (high token cost in
    tool-using mode)
  - You want zero contamination of the target session's records
  - You want deterministic re-runs (no LLM stochasticity from
    tool-call interleaving)
- Use the default tool-using mode when:
  - You want the watchdog to drill into specific causal evidence
    beyond what the structured `SessionOverview` summarises
  - You're tuning the `SessionOverview` itself and want to see what
    tools the watchdog would have called

If both modes disagree, that's a useful signal that the
`SessionOverview` is missing context the tool-using watchdog found —
the right fix is to extend the overview rather than rely on tools at
auto-invoke time.

### 2.4 Side-effect contamination

The watchdog has live tools (`agent_message`, `session_escalate`).
Running it against real archived sessions will write **real**
`agent_messages` and `user_interactions` rows targeting those sessions.
That contamination is normally harmless (the targets are archived) but
the operator should be aware. To clean up after a run:

```sql
-- in gateway.db
DELETE FROM agent_messages
  WHERE sender_session_id = 'gateway:sentinel'
    AND created_at >= '<experiment-start-timestamp>';

DELETE FROM user_interactions
  WHERE turn_id LIKE 'turn-%'
    AND agent_id IN (<diverged-session-agent-ids>)
    AND created_at >= '<experiment-start-timestamp>';
```

A future iteration could add a `--dry-run` flag to the watchdog that
stubs out the messaging tools; for now we accept the contamination
because the experiment is one-off.

## 3. Decision rule

The harness computes a 2×2 confusion matrix for **each judge** (Layer 1
alone, Watchdog alone):

|                 | Predicted positive | Predicted negative |
|-----------------|--------------------|--------------------|
| Actual diverged | TP                 | FN                 |
| Actual succeeded| FP                 | TN                 |

From these:

- **TPR (recall)** = TP / (TP + FN) — fraction of diverged sessions
  caught.
- **FPR** = FP / (FP + TN) — fraction of healthy sessions falsely
  flagged.
- **Precision** = TP / (TP + FP) — when the judge flags, how often it
  is right.

**Success criteria for auto-invoke (both must pass):**

| Criterion | Threshold | Rationale |
|---|---|---|
| `TPR(watchdog) − TPR(layer 1) ≥ 0.20` | +20 percentage points | The watchdog has to clearly outperform the deterministic monitor on its primary axis; smaller gains do not justify the per-invocation LLM cost. |
| `FPR(watchdog) ≤ 0.10` | ≤ 10% | False positives erode planner / operator trust quickly. |

The decision rule is encoded in
`autonoetic/src/cli/sentinel_experiment.rs::decision_rule`. Tests pin
it so changes to thresholds are deliberate (`cargo test -p autonoetic
sentinel_experiment`).

### 3.1 Outcomes

- **GO** (both criteria pass) → ship auto-invoke as a P5 follow-up,
  default OFF, opt-in via gateway config.
- **NO-GO** (either criterion fails) → P3 stays manual-only. The
  watchdog remains a tool the operator runs on demand. Re-run the
  experiment after material changes to either layer or to the watchdog
  prompt.

A borderline result (e.g., TPR delta = 0.18, FPR = 0.05) is
**inconclusive** — file a follow-up to expand the corpus to N = 40 and
re-decide.

## 4. Output format

The harness writes a markdown report containing:

1. **Header** — generation timestamp, corpus size.
2. **Decision** — verdict, TPR delta, watchdog FPR, per-criterion pass
   marks.
3. **Confusion matrices** — one each for Layer 1 and Watchdog, with
   TPR / FPR / Precision.
4. **Per-session table** — session_id, label, Layer 1 level, both
   flagged-flags, watchdog outcome tag, wall-clock seconds.
5. **Parse-error list** — any corpus entries that failed to parse.
6. **Watchdog replies** — truncated to 4 KB each, grouped by session.

Sample table column for one row:

```
| session_id | label    | L1 level | L1 flagged | WD flagged | WD outcome | Wall (s) |
| abc12345…  | diverged | critical | yes        | yes        | completed  | 7.3      |
```

## 5. What this experiment does NOT measure

Be explicit about scope so the result is not over-claimed:

- **Severity calibration.** A "flagged" verdict treats `diverging` and
  `critical` identically. Distinguishing them needs a finer protocol.
- **Time to detection.** Whether the watchdog flags earlier than
  Layer 1 within a single session is not measured — both judges see
  the whole session's final state.
- **Cost / latency.** The wall-clock seconds per watchdog run is
  reported but not entered into the decision rule. A future iteration
  could add a cost ceiling (e.g., reject GO if median wall > 30s).
- **Real-world novelty.** The corpus is what the operator has on
  disk. Watchdog performance on unseen failure modes is not predicted
  by this experiment.

## 6. Reproducibility

Each report row records the watchdog's full reply (truncated to 4 KB)
so a reviewer can independently verify the keyword classifier's
verdict. To re-run the analysis without re-spending LLM tokens:

```bash
autonoetic sentinel-experiment \
  --corpus path/to/corpus.yaml \
  --output path/to/results-v2.md \
  --skip-watchdog
```

In `--skip-watchdog` mode the harness uses each corpus entry's
`cached_watchdog_reply` field. To populate it, take the per-session
replies from a prior real run and copy them into the YAML.

## 7. Results

**(to be filled in after the corpus run)**

| Run date | Corpus size | TPR Δ | WD FPR | Decision | Report |
|---|---|---|---|---|---|
| _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ |

## 8. References

- `docs/proposals/divergence-sentinel.md` §6 — the protocol this
  document operationalises
- `autonoetic/src/cli/sentinel_experiment.rs` — implementation
- `autonoetic/src/cli/watchdog.rs::run_watchdog` — programmatic entry
  point the harness invokes
- `autonoetic-gateway/src/runtime/trajectory_health.rs` — Layer 1
  signal types and aggregation rule
- Issue [#243](https://github.com/mandubian/autonoetic/issues/243) —
  tracking
