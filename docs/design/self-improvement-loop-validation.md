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
3. Sign off (or reject) the milestone in this document's §7.

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
| Manifest `capabilities` parameter widened/narrowed (e.g. `ReadAccess` scopes `["self.*"]` → `["*"]`) | ❌ A/B replay rejected with `surface_drift_rejected` (detected via `compute_capability_delta`) |

The guardrail is a **safety floor**, not a substitute for the operator
deciding what constitutes a meaningful prompt change. The intent: catch
mistakes where someone widens privileges without realising the loop
treats that as a normal prompt edit.

To lift the guardrail (P5+ work): set
`improve.allow_capability_changes: true` in the gateway config (and
optionally `restrict_to_prompt_only: false` to disable the gate entirely).
Low-blast-radius capability changes are then permitted with a coerced
minimum holdout; high-blast changes remain rejected. Pin this in your
config the moment you've signed off on P4 below.

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

## 8. P5 — agent-level evolution cycles

> Status: code shipped, awaiting **2 capability-change cycles + sign-off**.
> Tracking issue: [#250](https://github.com/mandubian/autonoetic/issues/250).

### 8.1 What changed

P5 extends the surface-change gate from binary (allow vs reject) to a
three-state policy:

| Candidate vs baseline | Policy | Holdout |
|---|---|---|
| No capability or tool-tier delta | `no_delta` → proceed | caller-supplied (default 0.3) |
| Delta, `allow_capability_changes = false` (default) | `Reject(prompt_only_violation)` | n/a |
| Delta, **opted in**, but adds or broadens a high-blast kind (`SandboxFunctions`, `NetworkAccess`, `CodeExecution`, `CredentialAccess`, `EmergencyStop`, `AgentRevision`, `SchedulerAccess`) | `Reject(high_blast_radius)` | n/a |
| Delta, opted in, low-blast | `capability_change_with_strict_holdout` → proceed | **coerced up to `capability_change_min_holdout` (default 0.5)** |

The tool response's `policy_applied` field surfaces which branch fired
so the operator audit trail is clear. When holdout is coerced, the
original value lands in `holdout_coerced_from`.

### 8.2 Cycle protocol (capability changes)

Same as §4, with two extra steps before validate:

1. Pick a real session whose weakness is **agent-level** — e.g., the
   planner can't reach evidence because `ReadAccess` scopes are too
   narrow, or it can't message an agent because `AgentMessage`
   patterns exclude the target.
2. `autonoetic improve --session <id>` — diagnosis + (identical)
   `Candidate` revision.
3. Edit the candidate's SKILL.md to **only** widen the capability the
   diagnosis pointed at. **Don't bundle prompt edits** in the same
   cycle — that defeats attribution.
4. Set `improve.allow_capability_changes = true` in the gateway
   config. Re-run `autonoetic improve`. The A/B replay should accept
   the comparison and coerce the holdout up.
5. Approve, deploy, monitor as in §4. The existing P-2.16 constitutional
   gate also fires at promote time as a second defender.
6. Fill in the row in §8.3.

### 8.3 Results (operator fills in)

2 cycles for P5 sign-off, on any 2 agents. Capability changes only —
prompt edits sit in §5.

| # | Date | Agent | Capability change | A/B verdict | Confidence | Deployed? | Post-deploy regression observed? |
|---|---|---|---|---|---|---|---|
| 1 | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ |
| 2 | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ |

### 8.4 Operator notes (P5-specific)

_TBD — surface anything specific to capability changes: gate
false-positives, holdout-coercion friction, high-blast classification
calls that felt wrong, etc._

### 8.5 Sign-off

| Status | Operator | Date | Notes |
|---|---|---|---|
| **GO / NO-GO** | _TBD_ | _TBD_ | _TBD_ |

A P5 GO unblocks #252 (P7 — progressive automation).

## 9. References

- `docs/design/self-improvement-loop-design.md` — full design context
- Issues [#249](https://github.com/mandubian/autonoetic/issues/249)
  (P4), [#250](https://github.com/mandubian/autonoetic/issues/250)
  (P5)
- `autonoetic/src/cli/improve.rs` — the CLI driving the cycles
- `autonoetic-gateway/src/runtime/tools/improvement.rs` — A/B replay
  tool + `evaluate_surface_change_policy`
- `autonoetic-gateway/src/runtime/eval_stats.rs` — multi-axis
  statistical comparison
- `autonoetic-types/src/config.rs::ImproveConfig` — four config
  flags: `restrict_to_prompt_only`, `allow_capability_changes`,
  `capability_change_min_holdout`, `high_blast_radius_capability_kinds`
