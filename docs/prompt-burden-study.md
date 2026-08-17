# Prompt Burden: what makes Autonoetic's prompts large, and what actually shrank them

**Status:** Complete for the levers listed here — 2026-08-14 → 2026-08-17.
Shipped across six PRs (§11). Tracking: [#1087]. This document absorbs the
former `docs/rfc/prompt-burden-phase-gated-guidance.md`: the design it proposed
is implemented, so the proposal and its results are now one record.

**Origin:** an operator observation that Autonoetic agents carry far larger
system prompts than comparable agent stacks, attributed to "all the rules we
impose". Measurement did not support that attribution, and the real causes turned
out to be elsewhere.

**Read this if** you are adding doctrine to a `SKILL.md`, writing a tool
description, or wondering why the prompt is the size it is. The practical rules
are §6; everything before it is the evidence for them.

**Structure:** §1–§6 are the findings and the rules that follow from them —
read those. §7–§9 are the mechanism design, risks and non-goals, for when you
need to change how gating works rather than use it. §10–§11 are status and
history.

Day-to-day authoring guidance lives in
[`agent-prompt-guidance.md`](agent-prompt-guidance.md); this document is the
evidence behind it.

---

## 1. Summary

The planner's fixed system prompt was ~27.9k tokens. It is now ~22.0k on the
turn that matters most, without deleting a single rule.

| Agent | modal turn, before | after | change |
|---|---:|---:|---:|
| `planner.default` | 27.9k tok | **22.0k tok** | **−21%** |
| `planner.collaborative` | 28.6k tok | **24.9k tok** | **−13%** |
| `coder.default` | 17.2k tok | 17.1k tok | −0.6% |

Three findings matter more than the numbers:

1. **The rules were never the problem.** Foundation + guidance — every
   constitutional framing, right, and piece of centralized doctrine — was ~4.8k
   of the planner's ~27.9k. Deleting all of it, which we cannot and should not,
   would have bought 17%.
2. **Ownership beat compression by roughly 7:1.** Asking *"should this agent be
   carrying this at all?"* returned 9% in one change. Asking *"how do we carry it
   more cheaply?"* returned 1.3% across a whole migration pass.
3. **Nothing measured the prompt.** That is why it reached 27.9k unnoticed, and
   it is the finding most likely to matter in a year. It is now measured and
   ratcheted in CI.

---

## 2. What the prompt is actually made of

Measured by `autonoetic-gateway/tests/prompt_composition_budget.rs`, which
composes real prompt inputs from the repo's own `SKILL.md` files plus the live
tool registry:

```bash
cargo test -p autonoetic-gateway --test prompt_composition_budget -- --nocapture
```

The planner at the start of the study:

| Layer | tokens | share |
|---|---:|---:|
| Tool schemas (35 tools) | 11.6k | 41% |
| `SKILL.md` (core + extended) | 11.2k | 40% |
| Foundation layers | 3.3k | 12% |
| Guidance blocks | 1.6k | 6% |
| Output contract | 0.3k | 1% |

### 2.1 Three measurements, not one

An agent pays a different amount depending on where it is in a session, and a
lever that moves one number can leave the others flat:

| Measurement | What it is | Which levers move it |
|---|---|---|
| **turn 1** | before the extended `SKILL.md` half is inlined | `<!-- extended -->` split, exclusions, tiering |
| **working** | extended loaded, no phase fact earned — **the modal turn** | section gates (eviction), exclusions, ownership |
| **steady state** | after `artifact_built` | exclusions, ownership |

This distinction was learned the hard way: P3 initially looked like a no-op
because its effect falls entirely between turn-1 and steady-state. **A lever
whose effect falls between your measurements looks like a no-op.**

### 2.2 It was not duplication

A 12-gram shingle analysis across all 32 `SKILL.md` files plus the 6 foundation
layers found **65 shared shingles** in total. The #466 doctrine-migration worked;
there was no second consolidation pass waiting. The single remaining copy-paste
is *"Start working immediately on turn 1…"* in 8 files — ~350 tokens fleet-wide.

---

## 3. What worked

Ranked by return, which is not the order anyone would guess.

### 3.1 Ownership — 9% from one change

**The rule:**

> Before compressing a heavy tool schema, check whether the agent holding it has
> the **full capability set** for the flow the tool belongs to. If it must bounce
> to another agent mid-flow, the tool is in the wrong place — and moving it beats
> compressing it.
>
> **Qualifier:** a mid-flow bounce marks a misplacement *only when no privilege is
> being contained by it*. If the bounce exists so the holder **cannot** do
> something, it is a security boundary and must stay.

**The case.** `credential_setup` was the heaviest tool schema in the fleet (13%
of the planner's), and the planner's Decision Flow spent ~3.6k chars driving it.
But the planner lacks `NetworkAccess` while cold-start onboarding inherently
needs to fetch a skill spec — so its `SKILL.md` documented a bounce to
`researcher.default` and back. That is a misplaced responsibility showing up as
prompt size. `credential_onboarding.default` already held
`CredentialAccess` + `NetworkAccess` + `WriteAccess` on `skills/*`.

Result: planner steady state 110,015 → 100,103 ch (−9%), turn 1 −12%.

**The qualifier came from the counter-example.** `coder.default` returns
`needs_packager` because it lacks `NetworkAccess` — also a bounce, but a correct
one: the no-network sandbox is a deliberate boundary (hermetic tests, P-3.10).
Granting it network to remove the bounce would trade a real invariant for tokens.
Credentials qualified because the receiving specialist held the *same*
`CredentialAccess` — nothing was contained, only fetched.

**A transfer only counts if the receiver is scoped.** `credential_onboarding`
had no `excluded_tools` at all and advertised 41 tools for a credential job. It
gained a scope list at the same time (41 → 26) and ended up *smaller* than before
it absorbed the ceremony.

### 3.2 Eviction — 10.9% of the modal turn

Phase-gated `SKILL.md` sections, declared in frontmatter:

```yaml
sections:
  - heading: "Evaluation Federation"
    when: phase(artifact_built)
```

`<!-- extended -->` **defers** (the extended half inlines permanently from turn
2, so it saves exactly one turn). A section gate **evicts**: absent until the
session reaches the phase. A planner that never builds anything never pays for
the federation doctrine.

Result: planner working state 98,769 → 88,037 ch (−10.9%).

### 3.3 Scoping the tool surface — several thousand chars, nearly free

`excluded_tools` and `progressive_tool_disclosure` already existed and were
barely used — `allowed_tool_tiers` was declared by **1 of 32** agents. Aligning
`planner.collaborative`'s exclusions with `planner.default` and scoping
`credential_onboarding` cost no new mechanism at all.

### 3.4 De-duplication inside tool definitions — unconditional, small

Heavy tool descriptions largely restated their own schema. `credential_setup`
repeated its `skill_url` field verbatim; `resolve` repeated its entire pagination
paragraph in the `offset`/`limit` field descriptions — paid by every agent
holding `ReadAccess`. Deleting the restatement costs nothing.

Related: **rules the gateway already enforces belong in the rejection.**
`credential_setup`'s "never collect secrets via `user_input`" is enforced, and
the rejection already carries a repair hint with the exact replacement step. A
self-explaining enforced rule does not need pre-loading into every turn.

---

## 4. What did not work

Recorded so it is not retried hopefully.

### 4.1 Moving procedure into `ToolPresent` guidance blocks — a token wash

The founding plan was: apply a signature/procedure test to tool descriptions,
move the procedure into `ToolPresent`-gated guidance blocks, expect planner tool
schemas 11.6k → ~7k tokens.

**A `ToolPresent` block fires exactly when the tool is advertised — i.e. exactly
when its description would have been in the prompt.** Relocating prose between
them is a wash. The first `credential_setup` attempt came out **+743 chars**
because the block was slightly longer than the text it replaced.

Only `federation_escalate` paid, and only because it was gated on
`Phase(artifact_built)`, not merely on `ToolPresent`. Generalizing from it was
the error.

### 4.2 P4 (failure tables → errors) — not a reduction lever

Projected ~2.2k tokens. Delivered **+60 chars**.

Two things went wrong with the plan. First, having the gateway emit *where* to
route a failure is an opinion, and declarative `on_failure` fallback was already
rejected (#14) in favour of the gateway staying mechanical. Second, the tables
could not simply be deleted: most rows encode routing that is genuinely the
planner's, and only the rows restating typed fields were removable — after which
stating the wire contract *correctly* (snake_case values, optional fields) cost
back what those rows freed.

Its real value was removing a drift hazard: the gateway already emits
`failure_class`, `retry_advice`, `side_effect_state`, `agent_outcome`, and the
`SKILL.md` carried a parallel prose classification that matched on error strings.
**Two classifications of one failure, one typed and computed, one hand-maintained.**

### 4.3 The ownership audit's second pass — negative

Applying §3.1's rule fleet-wide found no second misplacement. `skill_install` is
declared by no agent (never advertised, zero cost); `artifact_prepare` sits on
`executor.default`, which holds the full set; `agent_revision_create_from_intent`
is pipeline delegation, not a capability bounce. **The credential case was
exceptional, not the first of many.**

### 4.4 Compression generally — 1.3%

The technique the effort was originally designed around returned the least. The
remaining tool-schema weight is *signature*, not procedure, so no amount of prose
migration reaches it (see OQ5 in the RFC).

---

## 5. The recurring failure mode: silent drift

Every serious defect found during this work was the same shape — **two sources of
truth for one thing, one of them prose, drifting quietly**:

| Instance | Found by |
|---|---|
| Planner failure table diverged from `failure_classification.rs` | reading the code |
| Three `SKILL.md` files disagreed about what `credential_onboarding.default` does | the ownership work |
| Doctrine naming Rust variants (`DoNotRetry`) the wire never emits (`do_not_retry`) | review |
| Cache-boundary comment claiming byte-identity that was already untrue | review |
| `skill_install` silently dropping declared section gates | review |

None were visible until someone read the code. This is why:

- **Section gates live in frontmatter, not inline markers.** Inline is more
  discoverable, but a marker that drifts from a renamed heading stops matching
  silently. Frontmatter gates are validated at parse time against the heading
  list *and* the phase-fact vocabulary, and reject duplicates.
- **The budget harness exists at all.** `context_governor` treats
  `system_prompt_tokens` as a constant, so nothing measured the prompt and every
  doctrine addition was free at the point of authorship.

---

## 6. Practical rules for adding doctrine

Apply in order. The first two are new; the third predates this study.

1. **Ownership.** Does the agent holding this tool have the full capability set
   for the flow? If it bounces mid-flow — and no privilege is being contained —
   move the tool, do not compress its description.
2. **Phase.** If most sessions never reach the situation this prose describes, it
   should not be in the prompt from turn 1. Gate it: a `Phase` guidance block for
   call mechanics, a frontmatter `sections:` gate for role doctrine.
3. **Litmus (pre-existing).** If two agents would write the same sentence, it
   belongs in neither `SKILL.md` — it is foundation or a guidance block.

And three things not to do:

- **Do not move prose into a `ToolPresent` block for size.** It is a wash. Move
  it for organisation, or gate it on a phase for size — not the former hoping for
  the latter.
- **Do not restate a schema field in its own tool description**, and do not
  pre-load a rule the gateway enforces with a repair hint.
- **Do not name internal types in doctrine that asks an agent to branch on a
  payload.** State serialized values (`"do_not_retry"`, not `DoNotRetry`), and
  say whether a field can be absent.

### 6.1 The budget is enforced

`prompt_composition_budget` enforces turn-1, working and steady-state ceilings
for six agents. **A failure is not a request to raise the number** — it means an
addition is being paid on every turn, and the question is whether it earns that.
Ceilings ratchet down as work lands; they are not raised to accommodate new
prose.

---

## 7. The mechanisms

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

### 7.1 Three properties that make it safe

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
this study is trying to make cheaper. So any result carrying artifact evidence
advances the phase, whoever produced it. (`SessionPhase::observe`, with a
depth-bounded scan.)

### 7.2 Persistence

`SessionPhase` is checkpointed. Losing it on resume would silently strip
procedure from the prompt at exactly the point the work is most advanced — the
worst possible moment. `#[serde(default)]` + skip-if-empty, so pre-existing
checkpoints load as "no phase yet".

### 7.3 Placement and order are part of the design

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
`session_phase`. The first three predate this work — the cache-boundary comment in
`lifecycle.rs` claimed byte-identity that was already untrue — and it was
corrected to state the real invariant. Anything added above that boundary which
can toggle *back* would churn the cache every turn and does not belong there.

### 7.4 The evidence scan is allowlisted to the artifact domain

The evidence path (§7.1) reads *other tools'* results, so its surface is a
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
  at the source instead: §7.5.

### 7.5 Derivation at the source, for the path no scan can see

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

### 7.6 Section gates — the same axis for `SKILL.md` role doctrine

Guidance blocks (§7.1–§7.5) gate *mechanical* doctrine. Role doctrine lives in
the `SKILL.md` body and gets the same axis through **frontmatter-declared**
gates.

#### Syntax and validation

```yaml
sections:
  - heading: "Evaluation Federation"
    when: phase(artifact_built)
```

A gate names a top-level `##` heading and the phase that must be reached before
that section enters the prompt. It carries its subsections with it, so `###`
children move with their parent and cannot be orphaned.

**Why frontmatter over an inline marker.** An inline marker is more discoverable
— you see it while editing the section — but it can drift from a renamed heading
and simply stop matching, after which the section silently loads always or never.
That is the exact failure mode this effort has hit repeatedly (a doctrine table
diverged from `failure_classification.rs`; three `SKILL.md` files disagreeing
about one agent; doctrine naming Rust variants the wire never emits). Frontmatter
gates are validated in `SkillParser::parse`, which rejects:

- a gate naming a heading the body does not contain (and lists the headings it
  did find);
- a gate naming a phase fact the gateway never derives (validated against
  `ALL_PHASE_FACTS`, and lists the known ones);
- an unparseable `when`.

The discoverability gap is cheap to close — the parse error names the agent and
the heading. The validation gap in the inline form is not: you would end up
building this validator anyway, against a weaker source of truth.

#### Eviction, and where earned sections render

`<!-- extended -->` **defers**: the extended half is inlined permanently from
turn 2, so it saves exactly one turn. A section gate **evicts**: the section is
absent until its phase is reached.

Earned sections render in the **phase tail**, next to phase guidance — not back
in their original position. Re-inserting in place would shift every cached byte
after the insertion point; appending keeps prefix growth append-only (§7.3).
Within the tail they are ordered by fact arrival, same as guidance.

Compose-time is deliberately forgiving where parse-time is strict: a gate whose
heading is missing is *ignored* during composition rather than failing closed,
because failing closed would silently strip prose from a live session. The error
belongs at parse time, where it can name the file.

#### The metric this exposed

Applying gates to the planner's federation cluster changed neither headline
total, because both gated sections live in the **extended** half — already absent
at turn 1, and legitimately present in the post-`artifact_built` steady state.
The win lands between those two points, on the **modal turn**: extended loaded,
no phase reached, which is every turn of a session that never builds anything.

The harness gained a third measurement for it, `working_chars()`:

| planner.default | before | after |
|---|---:|---:|
| turn 1 | 72,000 ch | 72,000 ch (unchanged) |
| **working (no phase reached)** | 98,769 ch | **88,037 ch (−10.9%)** |
| steady state (`artifact_built`) | 100,161 ch | 100,161 ch (unchanged) |

**A lever whose effect falls between your measurements looks like a no-op.** P3
was nearly recorded as one. Both prior levers (extended split, tool exclusions)
moved turn-1 or steady-state, so those were the two numbers the harness tracked;
eviction moves neither. All six agents now carry a working-state ceiling
alongside the other two.

---

## 8. Risks and how each is contained

**A phase never fires and the agent is stranded.** Mitigated by construction:
the signature half always stays in the schema, so the tool remains callable; the
gate is set *earlier* than the first turn the tool could succeed (an artifact
must exist before escalation is meaningful); and evidence-based derivation
(§7.1) covers the delegating-agent case that would otherwise be the common
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

## 9. Non-goals — what was never on the table

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

## 10. Where it stands

Remaining items are small or re-scoped — see [#1087] for live status.

- **P5 (tiering)** — largely overtaken by `excluded_tools` plus the live
  `progressive_tool_disclosure` flag. Confirm `allowed_tool_tiers` still adds
  anything before spending effort.
- **P4 remainder** — re-scoped as a correctness change with ~neutral token cost.
- The semantic-intent install path (`agent_revision_create_from_intent`) cannot
  express section gates.

### 10.1 Smaller items surfaced by the analysis

**a.** *"Start working immediately on turn 1…"* appears verbatim in 8
`SKILL.md` files. It is universal doctrine and belongs in `builtin_blocks()`.

**b.** `foundation_workflow.md` §14 (Clarification Protocol) substantially
restates the `clarification.ask_or_default` builtin block. One of the two should
go; the block is the better home because it is `Always` and already deduped.

**c.** `foundation_workflow.md` §7 and `foundation_artifact.md` §10 both carry
the content-handoff rule ("Do NOT return file contents in your response"). Agents
with both layers — every artifact-capable delegator, including the planner — get
it twice.

---

### 10.2 Open questions

1. ~~**Section-gate syntax for `SKILL.md` (P3).**~~ **Resolved: frontmatter**
   (see §7.6). Inline markers are more discoverable, but a marker that drifts from
   a renamed heading fails silently — the section then loads always, or never,
   with nothing to notice. Frontmatter gates are validated at parse time against
   both the heading list and the phase-fact vocabulary. Silent drift has been the
   recurring failure mode of this whole effort, so validatability won.
2. **Should `repair_hint` routing (P4) be data or prose?** A structured
   `{class, suggested_route}` is machine-checkable and testable against the
   enforcement register; prose is what the model actually acts on today.
3. **Does the phase vocabulary want a `no_build_intent` fact** — the negative
   case, letting a pure-Q&A session shed *more* than it currently can? This is
   the only place where a non-monotonic signal would genuinely pay, and it needs
   its own analysis before being adopted.
4. ~~**Derive the phase at the source, not from tool results.**~~ **Resolved**
   (see §7.5). The child-state notification path is now first-class; the tool
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
     mid-session schema change re-embeds everything after it — the problem §7.3
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

---

## 11. Appendix: what shipped, in order

| PR | What | Result |
|---|---|---|
| [#1084] | Phase axis, `SessionPhase`, one migrated tool, the measurement harness | made the prompt measurable |
| [#1085] | `planner.collaborative` exclusions + extended split | collaborative turn 1 −11.6% |
| [#1086] | Derive-at-source, budget ratchet, doc refresh | completed the mechanism on its primary path |
| [#1089] | P2 compression pass **and its correction**; credential ownership move | −1.3% then **−9%** |
| [#1098] | Ownership audit (negative), P4 first slice | drift fix, +60 ch |
| [#1100] | P3 section gates | working state −10.9% |

[#1084]: https://github.com/mandubian/autonoetic/pull/1084
[#1085]: https://github.com/mandubian/autonoetic/pull/1085
[#1086]: https://github.com/mandubian/autonoetic/pull/1086
[#1089]: https://github.com/mandubian/autonoetic/pull/1089
[#1098]: https://github.com/mandubian/autonoetic/pull/1098
[#1100]: https://github.com/mandubian/autonoetic/pull/1100
[#1087]: https://github.com/mandubian/autonoetic/issues/1087
