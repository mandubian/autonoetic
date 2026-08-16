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

### 3.3 Placement and order are part of the design

A fact landing mid-session must be a pure **append** to the cache prefix.
Anything else invalidates every cached byte after the insertion point. Two
decisions make that true, and only the second is obvious:

**Placement — earned blocks render last, after the output contract.** Composition
order is foundation → guidance → bridging → persona → user context → `SKILL.md` →
output contract. The standing guidance section sits *early*, followed by the
agent's entire `SKILL.md` (~11k tokens for the planner). An earned block rendered
there re-caches all of it **regardless of its position within the section** —
ordering inside the guidance section buys nothing on its own. So
`compose_guidance` returns two sections ([`ComposedGuidance`]) and
`compose_system_instructions_full` renders the phase tail at the very end of the
cache prefix. Guarded by `phase_guidance_renders_after_the_standing_prompt`.

**Order within the tail — by fact arrival, not priority or id.** Sorting by block
id (the natural tie-break) means a block earned at fact 3 can render ahead of one
earned at fact 1, which is an insertion, not an append. So `SessionPhase` holds
facts in arrival order and the tail sorts by activation index — `max` for `All`
(a block appears when its *last* required fact lands), `min` for `Any`. Guarded by
`phase_tail_is_ordered_by_fact_arrival_not_id_or_priority` and
`phase_tail_grows_by_appending_as_facts_land`, which asserts the tail at fact N is
a prefix of the tail at fact N+1.

`PHASE_GATED_PRIORITY_FLOOR` remains as a belt-and-braces guard — it keeps
priorities legible and makes accidental mis-gating visible in review — but
placement, not priority, is the mechanism.

The prefix is therefore **stable between milestones, not byte-identical for the
whole session**. Four monotonic flags shift it, each at most once:
`extended_loaded` (#1015), `tool_tier_escalated`, `discovered_tools`, and now
`session_phase`. The first three predate this RFC — the cache-boundary comment in
`lifecycle.rs` claimed byte-identity that was already untrue — and this branch
corrects it to state the real invariant. Anything added above that boundary which
can toggle *back* would churn the cache every turn and does not belong there.

### 3.4 The evidence scan is allowlisted to the artifact domain

The evidence path (§3.1) reads *other tools'* results, so its surface is a
contract every gated block inherits. It is restricted to an audited allowlist
(`tool_emits_artifact_evidence`): `artifact_*`, `promotion_*`, `federation_*`,
`agent_revision_*`, `workflow_*`, `workbench_*`, plus `resolve` and
`sandbox_exec`. Every tool there operates *on* artifacts, so a non-failed result
naming one is genuine proof — including `resolve`, where successfully resolving
an `ar.*` ref is itself the proof.

The bias is deliberate and asymmetric: **a missing block is recoverable** (the
agent still holds the tool signature, and tool errors carry repair hints), while
**a phantom block is paid by every session that trips it, forever**.

Two narrower scopings were considered and rejected:

- *Scan every result.* Sound against today's tool set — `agent_inspect` and
  `execution_search` emit no `artifact_ref` — but it makes the fact hostage to any
  future tool that echoes the key for unrelated reasons, at a point where many
  blocks depend on it.
- *Scan only `workflow_state` / `workflow_wait`.* Too tight, and it fails on the
  primary case. The gateway's child-state notification tells the agent verbatim
  *"you do not need to call `workflow_state`"* and arrives as a resume
  user-message, **not a tool result** — so a planner following the documented
  yield-based flow would never advance the phase this way. That path is handled
  at the source instead: §3.5.

### 3.5 Derivation at the source, for the path no scan can see

There are **two** derivation sites, and the tool scan is the lesser one.

When a child reaches a terminal state, the gateway wakes the parent with a
`child_state_notification` carrying the child's typed state — and tells it
verbatim *"you do not need to call `workflow_state`"*. That notification is
rendered into turn-start **messages**, so no tool result ever exists for
`SessionPhase::observe` to read. For a planner following the documented
yield-based flow, this is precisely the moment an artifact enters the workflow.

`SessionPhase::observe_gateway_signal` therefore advances the phase directly from
the signal payload, at every `gateway_signal_turn_start_context` call site. It
applies the same discipline as the tool path — gateway-observed state only, and a
child that did not succeed proves nothing, since a failed child can still name an
`artifact_ref` in its summary while having produced nothing.

Both signal shapes go through one predicate
(`child_notification_proves_artifact`): a standalone notification, and each
element of a join's `child_summaries` (which is a
`Vec<ChildStateNotification>` — the same per-child shape). Judging join children
*individually* is what stops a failed child in a mixed join from lending its
`artifact_ref` to the group; a whole-payload scan would allow exactly that.

One shape had to be handled for either path to see anything: a child's reply
travels as a **string** containing JSON (`summary: "{\"artifact_ref\":\"ar.x\"}"`),
in both notifications and joined `workflow_wait` results. The evidence scan
descends into strings that already look like JSON — the most common shape of "my
child produced an artifact" is otherwise invisible.

Without this, a yield-based planner earned the fact only *incidentally*, when it
later happened to call an artifact-domain tool.

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

**P1 — Instrument. ✅ Landed** (#1084, plus the follow-up completing §3.5 and the
budget ratchet). Harness + `Phase` mechanism + one migrated tool. The harness now
*enforces* a per-agent steady-state ceiling rather than only reporting, so prompt
growth costs something at the point of authorship instead of being invisible and
free. **The ceilings ratchet down as P2–P5 land; they are never raised to
accommodate a new addition.**

**P2 — Migrate the heavy tool descriptions. 🔄 In progress — and the original
premise was wrong.**

The plan said: apply the signature/procedure test, move procedure into
`ToolPresent`-gated blocks, expect planner tool schemas 11.6k → ~7k tokens.
Measuring the first migration showed **that move saves nothing**. A
`ToolPresent` block fires exactly when the tool is advertised — i.e. exactly
when its description would have been in the prompt. Relocating prose from a
description to such a block is a token *wash*; the first attempt at
`credential_setup` came out **+743 chars** because the block was slightly longer
than the text it replaced.

Corrected, there are three distinct levers, and only two of them pay:

1. **De-duplication — unconditional saving.** The heavy descriptions largely
   restate their own schema. `credential_setup` repeated its `skill_url` field
   verbatim, the per-variant semantics already in `credential_step_oneof_schema`,
   and the resume mechanics already on `credential_id`/`resume_vars`. `resolve`
   repeated its whole pagination paragraph in the `offset`/`limit` fields. This
   is pure waste and deleting it costs nothing.
2. **Rules the gateway already enforces belong in the rejection.** The
   `credential_setup` warning "never collect secrets via `user_input`" is
   enforced, and the rejection already carries a repair hint with the exact
   replacement step. A self-explaining enforced rule does not need to be
   pre-loaded into every turn — this is P4's principle applied to tool schemas.
3. **Phase gating — conditional saving, and only where a genuine precondition
   fact exists.** `promotion.record_protocol` and `promote.approval_continuation`
   both presuppose an artifact, so both moved to `Phase(artifact_built)`.

**What remains blocked.** The largest single item is `credential_setup`'s
`steps` `oneOf` schema (~2.8k chars). It is genuine signature — it tells the
model how to construct a valid argument — so it cannot move to a block, and it
is only needed on the programmatic path (the documented planner flow uses
`skill_url`). Shedding it needs **conditional schema shaping**: `definition()`
currently takes no context, so a tool cannot present a narrower schema on turns
where the wide one is irrelevant. That is the unlock for the rest of P2 and
should be designed before more tools are migrated — see Open Question 5.

Measured so far (this pass): planner tool schemas 46,224 → 44,734 ch; steady
state 111,689 → 110,199 ch. Real, but ~1.3% — not the 40% the original
projection implied. **The projection in §5's summary should be treated as
unproven until OQ5 is resolved.**

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

> **Caveat added after P2 began.** This projection assumed moving procedure out
> of tool descriptions saves tokens. It does not (see P2). Roughly 40% of the
> projected tool-schema saving depends on **conditional schema shaping** (OQ5),
> which does not exist yet. P3 and P4 are unaffected — they shed prose that is
> genuinely absent when gated. Treat the planner target as ~17–19k until OQ5 is
> resolved, and this line as the thing to re-derive rather than to trust.

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
4. ~~**Derive the phase at the source, not from tool results.**~~ **Resolved**
   (see §3.5). The child-state notification path is now first-class; the tool
   scan is the fallback rather than the primary mechanism.
5. **How should a tool shed schema that is irrelevant this turn?** Raised by P2:
   the remaining tool-schema weight is *signature*, not procedure, so no amount
   of prose migration reaches it. `credential_setup`'s `steps` `oneOf` is the
   type case — needed only on the programmatic path, while the documented planner
   flow passes `skill_url`.

   **Two findings sharpen the question.**

   *The soundness bar is lower than first stated.* Nothing validates tool
   arguments against the advertised schema: `validate_against_schema` serves
   agent I/O (`io.accepts` / `io.returns`), and each tool's `execute` deserializes
   its full `Args` regardless of what was advertised. No provider is sent
   `strict: true` either — `strict_schema_anyof` is only a *shape* accommodation
   for Moonshot/Kimi's schema validator, not opt-in strict function calling. So a
   narrow schema **cannot mechanically reject a legitimate wide call**. The real
   failure mode is *discoverability* — the model not knowing the option exists —
   which makes "narrow standing, widen on demand" safe provided the wide form has
   a mechanical pointer (`tool_discover` plus a `repair_hint` naming it).

   *De-dup does not dissolve the problem.* Running the P2 de-dup test inside
   `credential_step_oneof_schema` (branch prose restating the top-level
   description and the enforced secrets rule) recovered only **184 chars**. The
   residual is **structural** — four branches × their properties, plus the nested
   `secret_field_spec_schema` — roughly two-thirds of the tool's remaining
   4,881 chars. The weight cannot be written away.

   **Options.**

   - **(a) Context-aware `definition()`.** Rejected for this type case. Schemas
     sit in the cacheable prefix and cannot live in the phase tail, so a
     mid-session schema change re-embeds everything after it — the problem §3.3
     solved for guidance, reintroduced. Worse, there is no monotonic fact that
     correlates with "skill_url vs programmatic": it is an in-session usage
     decision, and `artifact_built` / `child_spawned` do not track it. Phase-gated
     schema would be arbitrary here.
   - **(a′) Static narrowing — `definition_for(&manifest)`.** Cheap to thread
     (the registry call sites in `lifecycle.rs` and `prompt_budget.rs` already
     hold the manifest, and an opt-in default keeps the 100+ impls untouched).
     But **name the qualifying agents before building it**: narrowing is only
     sound for an agent provably skill_url-only, and `planner.default`'s own
     SKILL documents both paths. If the list is empty, this is dead machinery.
   - **(b) Narrow standing schema + `definition_full()` via `tool_discover`.**
     Cache-free by comparison: discovery already changes the schema section, so
     narrow→wide piggybacks on an invalidation that happens anyway. Costs one
     round-trip whenever the wide form is needed.
   - **(c) Leave it.** Tool schemas floor out around 9–10k tokens for the planner.
     Still live: `credential_setup` is ~1.2k tokens ≈ 4.4% of planner steady
     state, so even perfect removal is far from the (already retracted) 40%.
   - **(d) Split the tool** — `credential_setup` (skill_url) plus a
     Specialized-tier programmatic variant, hidden from root sessions by
     progressive disclosure until escalation. Needs **no new mechanism**: it
     reuses tiering (P5) and `tool_discover`. Wrinkle: the resume path
     (`credential_id` + `resume_vars`) is common to both and would have to be
     duplicated or hosted on a third entry — possibly enough mess to sink it.

   **What decides (b) vs (c):** how often the programmatic path is actually used.
   (b) trades ~700 tokens per turn against one extra round-trip per session that
   needs the wide form. Rare → (b); routine → (c). That is a query against session
   history, not a judgement call, and it should be run before building anything.

   **This gates the remainder of P2.** Anything that lands must also survive the
   Moonshot/Kimi `anyOf`/`oneOf` sanitizer (`sanitize_schema_for_strict_anyof`) —
   a narrowed schema has fewer branches, not a different shape, so this is a
   check rather than an obstacle.
