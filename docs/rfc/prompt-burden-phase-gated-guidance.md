# RFC: Prompt Burden — Phase-Gated Guidance and the Cost of Always-On Doctrine

**Status:** Draft — 2026-08-14. Prototype in this branch (`feat/phase-gated-guidance`):
the `Phase` guidance condition, `SessionPhase` derivation, one migrated tool
(`federation_escalate`), and a measurement harness.

**Origin:** Operator observation that Autonoetic agents carry far larger system
prompts than comparable agent stacks, attributed to "all the rules we impose".
Measurement does not support that attribution — see §1 — and the real causes
point somewhere else.

**Related:** `docs/agent-prompt-guidance.md` (the three prose mechanisms),
`autonoetic-gateway/src/runtime/guidance.rs` (block mechanism, #463–#466),
`autonoetic-gateway/src/runtime/context.rs` (composition, foundation, extended
inlining #1015), `autonoetic-gateway/src/runtime/failure_classification.rs`
(P-5.14 mechanical classification),
`autonoetic-gateway/src/runtime/context_governor/` (the governor that trims
everything except the prompt), `docs/gateway-architecture-principles.md`.

---

## 1. Problem

The measurement harness added by this branch
(`autonoetic-gateway/tests/prompt_composition_budget.rs`) composes the real
prompt inputs for the two main agents from the repo's own `SKILL.md` files plus
the live tool registry:

```
cargo test -p autonoetic-gateway --test prompt_composition_budget -- --nocapture
```

```
=== planner.default — fixed system prompt ===
  tool schemas (35 tools)     46224 ch  (~11556 tok)
  SKILL.md core               17983 ch  (~ 4495 tok)
  SKILL.md extended           26709 ch  (~ 6677 tok)   [inlined from turn 2, permanently]
  foundation layers           13112 ch  (~ 3278 tok)
  guidance (pre-phase)         6265 ch  (~ 1566 tok)
  ---- turn 1 total           83584 ch  (~20896 tok)
  ---- steady-state total    111689 ch  (~27922 tok)

=== coder.default — fixed system prompt ===
  tool schemas (23 tools)     29576 ch  (~ 7394 tok)
  SKILL.md core                9931 ch  (~ 2482 tok)
  SKILL.md extended            9707 ch  (~ 2426 tok)
  foundation layers           13112 ch  (~ 3278 tok)
  guidance (pre-phase)         6308 ch  (~ 1577 tok)
  ---- steady-state total     68634 ch  (~17158 tok)
```

**The rules are not the problem.** Foundation + guidance — every constitutional
framing, every right, every piece of centralized doctrine — is ~4.8k of the
planner's ~28k steady-state prompt. Deleting all of it, which we cannot and
should not do, buys 17%.

**Nor is duplication the problem, any more.** A 12-gram shingle analysis across
all 32 `SKILL.md` files plus the 6 foundation layers finds **65 shared
shingles** in total. The #466 migration worked: there is no second
consolidation pass waiting to be run. The single remaining copy-paste is
*"Start working immediately on turn 1. Do not spend a turn acknowledging the
task"*, in 8 files — ~350 tokens fleet-wide, and an obvious `Always` builtin
block (§6.1).

The weight is in two places the existing mechanisms do not reach:

| Layer | planner | coder | Attacked by any existing mechanism? |
|---|---:|---:|---|
| Tool schemas | 41% | 43% | tiering — declared by **1 of 32** agents |
| `SKILL.md` (core + extended) | 40% | 29% | extended split — used by **2 of 32**, and it defers rather than evicts |

### 1.1 Three structural findings

**(a) Nothing measures the prompt.** `context_governor/trimming.rs` recomputes
totals as `system_tokens + conv + tool_tokens` and treats `system_prompt_tokens`
as a **constant**. The governor trims history and tools under pressure; the
fixed prompt is never reduced and never even reported. Every doctrine addition
has therefore been free at the point of authorship — no test noticed the prompt
growing. This RFC's harness is the minimum fix regardless of what else lands.

**(b) The extended-`SKILL.md` split is deferral, not reduction.**
`context::inline_extended` (#1015) injects the extended half on the agent's
FIRST tool call, and from then on it is part of the system prompt for the rest
of the session. Since essentially every planner turn makes a tool call, the
mechanism saves exactly one turn. The planner carries 6.7k tokens of extended
prose from turn 2 through turn N.

**(c) Every guidance condition is fixed at spawn.** `GuidanceCondition` gates on
`Capability`, `ToolPresent`, `ModelFamily`, `Role` — all decided when the agent
is spawned and constant thereafter. There is no way to express *"this prose
applies at this phase of the work"*. Because the only choices are "always" or
"never", everything that might matter at turn 40 is paid for at turn 1. This is
the missing axis, and it is why (a) and (b) have no lever to pull.

### 1.2 The shape of the waste

The planner's `federation_escalate` procedure, the carry-forward doctrine, the
credential-onboarding branches of the Decision Flow, the whole
`Evaluation Federation` section — none of these are wrong, duplicated, or
verbose for their content. They are simply **present in sessions that will never
reach them**. A planner answering "what does this repo do?" pays the full
promotion pipeline's doctrine.

That is the Hermes contrast, and it is architectural rather than editorial: a
lean agent stack keeps schemas terse and puts procedure in the *reaction* — the
error, the next tool result — instead of the *advertisement*. Autonoetic
front-loads everything because it has no way to say "later".

---

## 2. Non-goals

- **Deleting rules.** The constitution framing, rights, and `io.returns`
  contract stay. They are 17% of the prompt and load-bearing for the
  actors-as-citizens paradigm.
- **Shrinking `SKILL.md` role intent.** Decision logic and verdict rubrics are
  what a `SKILL.md` is *for*. The planner's Principles and Decision Flow spine
  should stay long.
- **A second doctrine-consolidation pass.** §1 shows there is nothing left to
  consolidate.
- **Lossy prompting.** No change may leave an agent unable to perform an action
  it could perform before. Every mechanism here is *deferral to the moment of
  need*, never removal.

---

## 3. Design: the phase axis

Add one condition whose truth changes *during* a session:

```rust
pub enum GuidanceCondition {
    // … existing, all fixed at spawn …
    Phase(&'static str),   // the session has reached this phase
}
```

backed by a monotonic, gateway-derived record:

```rust
pub struct SessionPhase { facts: BTreeSet<String> }
```

with the initial fact vocabulary:

| Fact | Proven by |
|---|---|
| `artifact_built` | `artifact_build` succeeded, **or** any result carries a non-empty `artifact_ref` / `reuse_guards.has_coder_artifact` |
| `gate_verdict_recorded` | `promotion_record` succeeded |
| `revision_seeded` | `agent_revision_create[_from_intent]` succeeded |
| `child_spawned` | `agent_spawn` succeeded |
| `credential_configured` | `credential_setup` succeeded |

### 3.1 Three properties that make it safe

**Derivation is mechanical (P-5.14, Lawful Executor).** Facts come from the
gateway's own record of what each tool returned. Agent prose never sets a fact,
so an agent cannot talk its way into extra guidance, and there is no reserved
judgment to leak.

**Facts are monotonic.** Nothing is ever retracted. A block that has entered the
prompt stays there, so the prompt prefix grows at most once per fact and remains
cacheable. A condition that could flip off would invalidate the provider's
prompt cache repeatedly and cost more than it saved.

**Evidence counts, not just authorship.** A planner never calls
`artifact_build` — its coder child does. Mapping tool names alone would leave
every lead agent permanently pre-phase, i.e. would fail precisely on the agent
this RFC is trying to make cheaper. So any result carrying artifact evidence
advances the phase, whoever produced it. (`SessionPhase::observe`, with a
depth-bounded scan.)

### 3.2 Persistence

`SessionPhase` is checkpointed. Losing it on resume would silently strip
procedure from the prompt at exactly the point the work is most advanced — the
worst possible moment. `#[serde(default)]` + skip-if-empty, so pre-existing
checkpoints load as "no phase yet".

---

## 4. What the prototype ships

Buildable and tested on this branch:

1. **`GuidanceCondition::Phase` + `SessionPhase`** (`runtime/guidance.rs`), with
   unit tests for derivation, failure handling, evidence-vs-authorship, and the
   checkpoint round-trip.
2. **Lifecycle wiring** — observation sits beside the existing
   `tool_tier_escalated` escalation in the tool-batch loop, and the phase is
   passed into `GuidanceContext`. Emits `autonoetic::session_phase` traces so the
   transition is visible in a real session.
3. **Checkpoint persistence** (`runtime/checkpoint.rs`).
4. **One migrated tool — `federation_escalate`.** Its `description` and its
   `revision_id` schema field carried a full procedure: when to call it, the
   seeded-vs-unseeded choice, placeholder-id warnings. Definition source shrinks
   **4630 → 3771 chars (−18%)**; the procedure returns as a
   `federation.escalate_procedure` block gated on
   `All(ToolPresent, Phase(artifact_built))`. A planner that never builds
   anything never pays for it; one that does gets it the turn an artifact
   appears — earlier than the turn it could possibly call the tool.
5. **The measurement harness** (§1), with three assertions: tool schemas are the
   largest single layer (a canary on this RFC's lever ordering); the procedure is
   absent pre-phase and present post-phase; and an agent without the tool
   (`coder.default` excludes `federation_*`) never sees the procedure in **any**
   phase — phase gating must not become a back door around capability gating.

The prototype deliberately migrates **one** tool. The mechanism is the artifact
under review; the rollout is §5.

---

## 5. Rollout

Each phase is independently valuable and independently revertible.

**P1 — Instrument (this branch).** Harness + `Phase` mechanism + one migrated
tool. Merging this alone makes prompt growth visible in CI, which is the change
that stops the bleeding.

**P2 — Migrate the heavy tool descriptions.** By measured size, the candidates
after `federation_escalate` are `credential_setup`, `promotion_record`,
`agent_revision_create_from_intent`, `artifact_exec`, `skill_install`,
`content_patch`. The test is mechanical: *does this sentence tell the model how
to call the tool (signature — stays), or what to do around calling it
(procedure — becomes a gated block)?* Expected: planner tool schemas 11.6k →
~7k.

**P3 — Evict, don't defer, in `SKILL.md`.** Replace the one-shot
`<!-- extended -->` inline with phase-gated sections. The planner's federation
cluster (`Evaluation Federation` + `Carry-forward` + `Partial re-federation`,
~2.5k tokens) is gated on `artifact_built`; the credential-onboarding branches of
the Decision Flow (~900 tokens) on the onboarding intent. This requires a
per-section gate syntax in `SKILL.md`; the mechanism is the same `Phase`
condition, so the design cost is small and the authoring cost is the real work.

**P4 — Move the failure tables into the errors.** The planner's
`Failure Handling` + `Terminal signals` + `Stuck Tasks` +
`unable_to_evaluate` (~1.5k tokens) and the coder's `Permission Denied` +
`Artifact Execution Failure Handling` + `Persistent Test Failure` (~750 tokens)
are error→action routing tables, preloaded into every turn to serve a small
fraction of them.

`failure_classification.rs` **already** classifies failures as a pure function of
gateway-observed state (P-5.14), and `ToolError` already carries a `repair_hint`
field — but the classifier emits no routing hint today. Moving each table row
into the `repair_hint` of the error it describes removes the tokens *and* closes
a live drift hazard: a preloaded table diverges from code silently, a hint
emitted next to the classification cannot. This is the existing orchestration
philosophy — gateway mechanical, agent semantic at decision time — applied in the
prompt direction, where it has not been applied yet.

**P5 — Use the tiering that already exists.** `allowed_tool_tiers` is declared by
1 of 32 agents. Giving `planner.default` and `coder.default` explicit tiers drops
the Specialized tail. Cheap, orthogonal, and independently testable via the
harness.

Projected steady-state after P1–P5: planner ~28k → 13–15k, coder ~17k → 9–10k,
**without deleting a single rule**.

---

## 6. Smaller items surfaced by the analysis

**6.1** *"Start working immediately on turn 1…"* appears verbatim in 8
`SKILL.md` files. It is universal doctrine and belongs in `builtin_blocks()`.

**6.2** `foundation_workflow.md` §14 (Clarification Protocol) substantially
restates the `clarification.ask_or_default` builtin block. One of the two should
go; the block is the better home because it is `Always` and already deduped.

**6.3** `foundation_workflow.md` §7 and `foundation_artifact.md` §10 both carry
the content-handoff rule ("Do NOT return file contents in your response"). Agents
with both layers — every artifact-capable delegator, including the planner — get
it twice.

---

## 7. Risks

**A phase never fires and the agent is stranded.** Mitigated by construction:
the signature half always stays in the schema, so the tool remains callable; the
gate is set *earlier* than the first turn the tool could succeed (an artifact
must exist before escalation is meaningful); and evidence-based derivation
(§3.1) covers the delegating-agent case that would otherwise be the common
failure. P2 migrations must preserve this discipline — the review question for
each is "could the agent still make a correct call with only the signature?"

**Prompt-cache churn.** Monotonicity bounds it: each fact changes the prefix at
most once per session, and facts fire early in the work rather than repeatedly.

**Phase gating becomes a back door around capability gating.** Guarded by test:
`agents_without_the_tool_never_see_its_procedure`. Blocks are still collected
only from tools that survived the tier/capability filter, so a `Phase` condition
can only ever *further* restrict.

**Procedure arrives later than an agent's planning would like.** Real: a planner
may want to describe the promotion path before an artifact exists. The mitigation
is that *route selection* (Decision Flow, "which agent builds this") stays
always-on; only *call mechanics* are gated. If a P2 migration finds itself gating
routing knowledge, the split is wrong.

---

## 8. Open questions

1. **Section-gate syntax for `SKILL.md` (P3).** Frontmatter-declared section
   gates, or an inline marker like the existing `<!-- extended -->`? The latter
   is more discoverable to agent authors; the former is easier to validate.
2. **Should `repair_hint` routing (P4) be data or prose?** A structured
   `{class, suggested_route}` is machine-checkable and testable against the
   enforcement register; prose is what the model actually acts on today.
3. **Does the phase vocabulary want a `no_build_intent` fact** — the negative
   case, letting a pure-Q&A session shed *more* than it currently can? This is
   the only place where a non-monotonic signal would genuinely pay, and it needs
   its own analysis before being adopted.
