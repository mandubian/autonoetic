# Prompt Burden: what makes Autonoetic's prompts large, and what actually shrank them

**Status:** Study complete for the levers listed here — 2026-08-14 → 2026-08-17.
Shipped in #1084, #1085, #1086, #1089, #1098, #1100. Tracking: [#1087].

**Origin:** an operator observation that Autonoetic agents carry far larger
system prompts than comparable agent stacks, attributed to "all the rules we
impose". Measurement did not support that attribution, and the real causes turned
out to be elsewhere.

**Read this if** you are adding doctrine to a `SKILL.md`, writing a tool
description, or wondering why the prompt is the size it is. The practical rules
are §6; everything before it is the evidence for them.

**Design detail** for the mechanisms lives in
[`rfc/prompt-burden-phase-gated-guidance.md`](rfc/prompt-burden-phase-gated-guidance.md).
Authoring guidance lives in [`agent-prompt-guidance.md`](agent-prompt-guidance.md).

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

## 7. Where it stands

Remaining items are small or re-scoped — see [#1087] for live status:

- **P5 (tiering)** — largely overtaken by `excluded_tools` + the live
  `progressive_tool_disclosure` flag. Confirm `allowed_tool_tiers` still adds
  anything before spending effort.
- **P4 remainder** — re-scoped as a correctness change with ~neutral token cost.
- **§6 micro-items** in the RFC — the 8-file duplicated line, and two foundation
  overlaps.
- **OQ5 (conditional schema shaping)** — the remaining tool-schema weight is
  signature, not procedure. Urgency dropped once ownership moved the type case
  off the planner.
- The semantic-intent install path (`agent_revision_create_from_intent`) cannot
  express section gates.

[#1087]: https://github.com/mandubian/autonoetic/issues/1087
