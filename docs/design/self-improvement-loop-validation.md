# Self-Improvement Loop — P4 Validation Milestone

> Status: **Awaiting operator-side cycles (2026-05-22)**.
> Tracking: [#249](https://github.com/mandubian/autonoetic/issues/249).
> Sister design: [`self-improvement-loop-design.md`](./self-improvement-loop-design.md).

## 1. Purpose

P4 is the **first end-to-end exercise** of the closed self-improvement
loop on real prompt-level changes. The code prerequisites (P0–P3 +
the propose step in #266 + the `restrict_to_prompt_only` guardrail in
this PR) are in place. What remains is **operator action**:

1. Run **3 successful end-to-end cycles** across any 3 agents.
2. Capture the multi-axis deltas, operator notes, and any surprises.
3. Sign off (or reject) the milestone in this document's §5.

P5 (agent-level evolution) is intentionally gated on P4's outcome —
broader scope only after the basic loop is shown to work.

## 2. Pipeline under test

`select → diagnose → propose → validate → approve → deploy → monitor`,
all under the **prompt-only guardrail**:

- `select` — `autonoetic improve --session <id>` or
  `--last-sessions N --agent X`.
- `diagnose` — operator-readable summary of the session(s) printed by
  the CLI.
- `propose` — forks the current active revision as a `Candidate` in
  the GatewayStore (see `propose_improvement` in
  `autonoetic/src/cli/improve.rs`). The candidate starts byte-identical
  to baseline; **the operator hand-edits the candidate's `SKILL.md`
  before validate** to make the prompt change being tested.
- `validate` — `improvement.ab_replay` tool runs an eval suite over
  both revisions, computes the multi-axis `CompareRecommendation`.
  **Guarded:** `improve.restrict_to_prompt_only = true` (default)
  rejects pairs whose declared capability or tool-tier surface differs.
- `approve` — CLI prompts the operator interactively unless
  `--no-prompt` was passed.
- `deploy` — on approval, the candidate is promoted via the existing
  `agent_revision_promote` path.
- `monitor` — the next N sessions for the affected agent are watched
  (existing post-promotion review machinery).

## 3. Guardrails

The PR that ships this document also adds `improve.restrict_to_prompt_only`
to `GatewayConfig`. Default: `true`. Behaviour:

| Candidate diff vs baseline | Guardrail outcome |
|---|---|
| Prompt / instructions body changed; manifest `capabilities` and `allowed_tool_tiers` unchanged | ✅ A/B replay proceeds |
| Manifest `capabilities` differ (variant added/removed) | ❌ A/B replay rejected with `surface_drift_rejected` |
| Manifest `allowed_tool_tiers` differ | ❌ A/B replay rejected with `surface_drift_rejected` |
| Manifest `capabilities` parameter changed (e.g. SandboxFunctions allowlist edit) | ✅ A/B replay proceeds (intentionally — parameter tuning is not surface widening) |

The guardrail is a **safety floor**, not a substitute for the operator
deciding what constitutes a meaningful prompt change. The intent: catch
mistakes where someone widens privileges without realising the loop
treats that as a normal prompt edit.

To lift the guardrail (P5+ work): set
`improve.restrict_to_prompt_only: false` in the gateway config. Pin
this in your config the moment you've signed off on P4 below.

## 4. Cycle protocol

For each of the 3 cycles, the operator does:

1. Pick a real recent session that exposed a prompt-level weakness
   (e.g., a planner that misread a tool's error message; a coder that
   missed a constraint stated in the user message).
2. `autonoetic improve --session <id>` — produces the diagnosis and a
   `Candidate` revision (initially identical to baseline).
3. Edit the candidate's SKILL.md prompt in
   `<gateway_dir>/revisions/agents/<agent>/<candidate_rev>/SKILL.md`.
   **Do not touch the `capabilities` or `allowed_tool_tiers` blocks.**
4. Re-run `autonoetic improve --session <id>` (or the appropriate
   resume) so the A/B replay picks up the edited candidate.
5. Inspect the comparison report. Note the per-axis deltas and the
   `CompareRecommendation` (PreferA / PreferB / Inconclusive).
6. Approve or reject when prompted.
7. If approved + deployed, run the affected agent on 2–3 unrelated
   sessions and watch for regression.
8. Fill in the row in §5.

## 5. Results (operator fills in)

Replace this section after each cycle. Cycles can be of any 3 agents
(planner, coder, researcher, etc.). Order is the order they're run.

| # | Date | Agent | What changed in the prompt | A/B verdict | Confidence | Deployed? | Post-deploy regression observed? |
|---|---|---|---|---|---|---|---|
| 1 | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ |
| 2 | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ |
| 3 | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ |

### Per-cycle multi-axis deltas

For each row above, attach the full `CompareRecommendation` JSON (from
the `stats` field of `eval_compare` / `improvement.ab_replay`'s output):

#### Cycle 1

```json
TBD — paste the full stats block from the CLI here
```

#### Cycle 2

```json
TBD
```

#### Cycle 3

```json
TBD
```

## 6. Operator notes (free-text)

After 3 cycles, the operator captures here what they want the rest of
the loop to know before P5 (agent-level evolution) starts.

### 6.1 What worked

_TBD — concrete observations. Avoid generalities; cite a specific
cycle when possible. e.g., "Cycle 2's prompt change reduced
turn-count by 38% with the same completion rate — the harness
correctly preferred B with 0.97 confidence."_

### 6.2 What was annoying

_TBD — friction points in the operator workflow. e.g., "The CLI
prompts for approval but doesn't show a diff of the candidate's
SKILL.md vs baseline — I had to `diff` it manually."_

### 6.3 What to change before P5

_TBD — concrete pre-conditions for P5. e.g., "Show a candidate
manifest diff in the approval prompt." / "Lift
`restrict_to_prompt_only` only for specific agents via an allowlist,
not globally."_

## 7. Sign-off

| Status | Operator | Date | Notes |
|---|---|---|---|
| **GO / NO-GO** | _TBD_ | _TBD_ | _TBD_ |

A GO signs off P4 and unblocks #250 (P5 — agent-level evolution). A
NO-GO requires filing follow-ups for whatever needs fixing first.

## 8. References

- `docs/design/self-improvement-loop-design.md` — full design context
- Issue [#249](https://github.com/mandubian/autonoetic/issues/249) —
  P4 tracking
- `autonoetic/src/cli/improve.rs` — the CLI driving the cycles
- `autonoetic-gateway/src/runtime/tools/improvement.rs` — the A/B
  replay tool + this PR's `restrict_to_prompt_only` guardrail
- `autonoetic-gateway/src/runtime/eval_stats.rs` — multi-axis
  statistical comparison
- `autonoetic-types/src/config.rs::ImproveConfig` — the
  `restrict_to_prompt_only` config flag
