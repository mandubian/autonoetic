# Operator Legibility — Realtime Control-Flow & Product Visibility

> Status: **Draft RFC** — concept agreement; slicing into PRs.
> Builds on: [`../archived/human-gate-unification-plan.md`](../archived/human-gate-unification-plan.md) (GateService),
> [`human-agent-artifact-collaboration-plan.md`](human-agent-artifact-collaboration-plan.md) (PlanFrame + workbench),
> [`operator-activity-feed-plan.md`](operator-activity-feed-plan.md).
> Rooted in: [`separation-of-powers.md`](../concepts/separation-of-powers.md), [`../concepts/separation-of-powers.md`](../concepts/separation-of-powers.md).

## 1. The problem, in one sentence

The operator can neither see the **decisions** (approvals / plan events scattered
across hundreds of routine rows) nor the **product** (content hidden until the
workbench ceremony) — so a session that *did the work* can still feel opaque and
stall on an invisible gate.

## 2. Case study: session `943eb58c` ("build a weather agent")

A 28-minute `planner.collaborative` session that produced a working audited
artifact (`ar.cc672ac6b137`, 8 files, audited at 13:19) **but never installed or
promoted it.** Reconstructed from `live_digest_events`:

| Time | Event | Note |
|------|-------|------|
| 13:00–13:03 | operator intent → researcher spawned | fine |
| 13:04 | `plan.pending` **v1** | `planframe_propose` |
| 13:05 | `plan.approved` v1 (operator) | gate 1 |
| 13:06–13:11 | architect + coder build the agent | fine |
| 13:11 | `plan.pending` **v2** ("s1,s2 done") | `planframe_amend` — re-gated |
| 13:15 | `plan.approved` v2 (operator) | gate 2 |
| 13:17 | packager + 3 evaluators spawned | fine |
| 13:19 | auditor audits the artifact | **the work is essentially done** |
| 13:21 | `escalation.pending` + `approval.pending` | federation `session_escalate` — gate 3 |
| 13:24 | `approval.approved` (operator) | |
| 13:27 | `plan.pending` **v3** ("all steps complete") | `planframe_amend` — re-gated, **never approved** |
| 13:34 | planner spawns `weather_forecast` to **test** it | despite the pending/unapproved plan v3 |
| 13:35 | test fails → planner spawns `researcher.default` for Tokyo | drift |
| 13:35 / 13:41 / 13:46 | `workflow_wait` × 3 ("second attempt", "third attempt") | **poll loop** — violates Ri-0.14 |
| — | no `promotion.*`, no `install.*` | **the agent was never activated** |

### What this reveals — three distinct failures, not one

1. **Re-gating churn.** `planframe_amend` unconditionally re-opens the operator
   gate (`runtime/tools/plan_frame.rs:904`, `:988`). v2 and v3 were *progress
   checkpoints*, not capability changes. Three of the four gates in this session
   were ceremony. Re-gating trivial amendments trains operators to rubber-stamp —
   which is the *actual* safety degradation.

2. **The gate's enforcement scope is wrong.** While plan v3 sat unapproved, the
   planner was *not* blocked from spawning agents and running ad-hoc tests. The
   gate blocked promotion but permitted drift. So today's gate is simultaneously
   too strict (re-gates nothing-burgers) and too loose (fails to keep the agent
   on the approved rail). A pending plan should constrain *lateral* action, not
   just the final promotion.

3. **Guidance/prompt drift (the factorized-prompt surface).** The planner
   abandoned the promotion path and entered a `workflow_wait` poll loop —
   explicitly forbidden by Ri-0.14 ("never re-issue `workflow_wait` in a loop").
   This is a guidance/mechanical-guard problem more than an approval problem,
   but it compounds: the operator sees a session that "went somewhere else" and
   never delivered. Tracked separately under §7.5.

The operator-facing symptom — "I don't know why it didn't promote" — is the
*intersection* of these: an unapproved cosmetic amendment (#1) that didn't
block drift (#2) that wandered into a poll loop (#3). No single fix solves it;
this doc addresses #1 and #2 and the visibility of all three.

## 3. Design principles

1. **Approve the envelope, execute within it.** Consent attaches to a
   *capability budget*, not to metadata churn. Re-approve only on budget
   expansion. This is a strict *strengthening* of the separation-of-powers
   invariant: the envelope becomes explicit and machine-checked.
2. **Decouple visibility from immutability.** Artifacts are immutable; *seeing*
   content as it is written does not compromise that. Drafts are mutable
   pointers over immutable blobs; immutability is an opt-in freeze.
3. **Make decisions and product independently legible, in realtime.** The
   timeline shows the *control flow* (checkpoints); a content surface shows the
   *product* (files). Neither requires waiting for a ceremony.
4. **Mechanical, never LLM-judged.** Every safety classification (envelope
   change, draft vs artifact, gate scope) is computed in the gateway. Consistent
   with existing `policy.rs` / GateService practice.

## 4. Pillar A — Timeline: squash + first-class checkpoints

### Status quo

Squash infrastructure **already exists**: `RowSource::Run { start, len }`, the
`squash` view flag (`cli/room/tui.rs:2464`), and the "collapsed run — N routine
events / press 's' to unsquash" detail (`tui.rs:3061`). What is missing is
*classification* and *elevation* — everything renders as a peer row.

### Proposal: three event tiers (pure view filter, no storage change)

| Tier | Examples | Rendering |
|------|----------|-----------|
| **Checkpoint** | `plan.*`, `approval.*`, `escalation.*`, `operator.message`, `session.start`, `workbench.created`, `promotion.*` | Always shown; banner chrome (icon + color + one-line gist); never squashed |
| **Significant** | state-changing tools: `agent_spawn`, `content_write`, `artifact_build/_project`, `sandbox_exec`, `federation_escalate`, `promotion_record`, `agent_revision_*` | Shown, compact |
| **Routine** | `turn.start/end`, `llm.round`, `agent.reasoning`, read-only tools (`resolve`, `artifact_inspect`, `agent_list`, `workflow_state/wait`) | Folded into runs |

- Default view = checkpoints + significant; routine folded into collapsible runs.
- `'s'` unsquashes a run (already wired). Add `'['` / `']'` to jump
  checkpoint-to-checkpoint so the approval *narrative* is scannable in seconds.
- Checkpoint banners carry a structured gist, e.g. an amber
  `◑ PLAN v2  Weather Forecast Agent — steps s1,s2 done` rather than a payload row.

### Checkpoint elevation example (this session)

Today the operator sees 554 interleaved rows. The tiered view collapses to
roughly:

```
● operator → "make it an agent…"            [13:03]
◑ PLAN v1  Weather Forecast Agent           [13:04]  → approved 13:05
▷ spawn architect → coder                   [13:06–13:11]
◑ PLAN v2  steps s1,s2 done (inherited)     [13:11]  → auto-approved
▷ spawn packager + 3 evaluators             [13:16–13:17]
● audit  ar.cc672ac6b137 (8 files)          [13:19]
◑ ESCALATION  session_escalate (federation) [13:21]  → approved 13:24
◑ PLAN v3  all steps complete (inherited)   [13:27]  → auto-approved
▷ test weather_forecast (Tokyo) ✗           [13:34–13:46]   ⚠ poll loop
● READY TO PROMOTE  →  (no promotion happened)
```

The drift and the missing promotion become *visible* rather than buried.

## 5. Pillar B — Plan amendments: inherit-by-default + visible diff

### The change

`planframe_amend` no longer unconditionally emits `awaiting_approval`. Instead:

- **Inherit by default.** A new revision is created in `approved` status with
  `inherited_from: parent_version`, *unless* a mechanical envelope diff detects
  a re-gating change (below).
- **Re-gate only on envelope expansion** (computed in the gateway, never by the
  agent): a new step; a new capability/skill; a sandbox elevation; a new
  remote-access target or principal; or scope growth (more agents, federation
  intent). This is the same mechanical-safety lens already used in `policy.rs`
  applied to plan revisions.
- **The diff is the object.** Plan revisions are immutable, so `parent ⊕ child`
  is computable. Store a `diff_summary` on the revision at amend time and render
  it as the checkpoint body:
  `v2 ← v1  +step s5 (packager)  cap:+remote_access(https)  owner:s3→operator`.
  Inheriting cases render `no envelope change — auto-approved`, so the operator
  can see *why* they were not asked.

### Reusing existing diff machinery

`content_patch` already carries two diff-relevant primitives:

- `compact_diff(old, new)` (`runtime/tools/content_patch.rs:660`) — currently a
  naive wholesale `- old / + new` dump, **not** a true line diff.
- the `v4a` patch parser (`*** Update File:` / `*** Add File:`) for multi-entry
  edits.

For plans we want a **structural** diff (steps ±/Δ, owners, capabilities,
sandbox, remote targets), not a textual one. Recommendation:

- Add a `plan_frame_diff.rs` that computes a structural envelope diff between
  two `PlanFrame` revisions and emits a `PlanEnvelopeDiff { steps_added,
  steps_removed, owners_changed, caps_added, sandbox_changed,
  remote_targets_added, federation_changed, requires_regate: bool }`.
- Upgrade `compact_diff` to a real LCS line diff and reuse it for free-text
  field deltas (reason / step descriptions), shared between `content_patch` and
  plan rendering. (Stand-alone cleanup; does not block this pillar.)

### Why this is *more* faithful to the constitution, not less

The principle is: operator consents to **risky / irreversible** change. A
progress checkpoint executes nothing new. Re-gating it both drowns real
envelope decisions in noise *and* (per the case study) fails to actually keep
the agent on the rail. Inherit-by-default makes the remaining gates meaningful.

## 6. Pillar C — Approve-the-envelope rationalization

### The reframe

> A plan approval **is** a capability budget. Tool calls spend against it
> silently. Re-approve only on budget expansion.

The dedup chain already has 4 layers (exec cache → session grants → pending/approved
domain matches → flood cap). This adds a 5th, **plan-scoped grants**, inserted
*ahead of* session grants:

1. When the operator approves a plan, materialize its declared envelope
   (capabilities, sandbox, remote targets, federation) into a **plan-scoped
   grant** in the existing `session_approval_grants` family (which already
   supports `ExactHost` / `HostSuffix` / `HostAndPort` / `UrlPrefix`, `expires_at`,
   and revocation).
2. Tool-level approval requests dedup against the plan grant first. The
   federation `session_escalate` in the case study would have been covered had
   the plan declared federation intent — no separate approval.
3. Plan envelope expansion (Pillar B) **revokes** the prior plan grant and
   issues a fresh one; that revocation is the mechanical event that forces
   re-approval.

This collapses the case-study session from **4 manual gates to ~1** (the initial
plan), with subsequent prompts only if the agent reaches outside the declared
envelope. Every `approval.*` checkpoint the operator sees is then a *real*
envelope decision, never ceremony — which is what makes Pillar A's checkpoint
timeline honest.

### Consistency note

This subsumes part of [`../archived/human-gate-unification-plan.md`](../archived/human-gate-unification-plan.md)
Phase "agent-as-decider (P-2.20/P-2.21)" by giving the *operator*-approved plan
a mechanical grant form, rather than delegating the decision to the agent. It
keeps the human gate first-class.

## 7. Pillar D — t=0 workbench: realtime product visibility

### The reframe that enables it

`content_write` already writes **content-addressed blobs** (immutable) registered
under **mutable names** (`runtime/tools/content.rs:111-112` — the name registry
is just a pointer). So "live files" are a *view* problem, not a storage problem.
Today visibility and immutability are coupled (you see only built artifacts);
this pillar decouples them.

### Session content tree (new pane/tab)

Every `content_write` creates/updates a virtual file (path = name, content =
latest blob pointed to). Live from t=0. Three states, visually unmistakable:

| Badge | State | Meaning |
|-------|-------|---------|
| `📝 draft` | latest blob, mutable by further `content_write` (pointer moves) | agent-written, un-vetted |
| `📦 artifact` | frozen via `artifact_build` (immutable, content-addressed) | built, reviewable |
| `🔒 pinned` | in a workbench checkpoint | operator-pinned for review |

- Selecting a file shows its content, rendered **markdown-aware** (reusing the
  pipeline fixed in the table/`@@NARRATIVE@@` work — `cli/room/markdown.rs` +
  `render.rs`).
- Selecting a draft with history shows the blob lineage (pointer over time) — a
  per-file mini-git. Drafts are explicitly labeled **"draft — not a vetted
  artifact"** so the operator never acts on a draft as promoted content.

### Workbench becomes a *mode*, not a late object

Today `workbench.created` fires at a ceremony (13:11 in the case study — 11 min
in, and only after plan v1 + architect + coder). Instead:

- The content tree exists from **t=0**.
- "Workbench" is *review mode* over that same tree — freeze selected drafts into
  a diffable checkpoint whenever the operator or agent wants. It is no longer a
  prerequisite for visibility; it is an opt-in freeze over content that was
  already visible.
- The workbench isn't late because it is no longer a prerequisite for anything.
  Build still produces an immutable artifact; the workbench just stops gating
  *seeing*.

### Why this also helps promotion (the case-study failure)

With a t=0 content tree + tiered timeline, the operator sees the audited
artifact sitting as `📦` at 13:19 *and* sees that no promotion followed. The
"why didn't it promote?" question becomes answerable from the screen, not just
from a post-mortem DB query. Combined with Pillar B (v3 auto-approves), the
promotion path is no longer blocked on a cosmetic gate that the operator forgot.

### Realtime auditability (forward look)

Because drafts are visible as they are written, an auditor/evaluator (or the
operator) can review code *while it is being produced*, not after a build. This
is the foundation for realtime/static audit hooks over the live tree — out of
scope for the first slice but enabled by it.

## 8. Tensions & decisions

- **Plan-diff correctness.** The mechanical envelope-diff must be conservative:
  when in doubt, re-gate. Start strict, relax with evidence. A false "no change"
  is worse than a false re-gate.
- **Draft vs artifact trust.** Drafts are agent-written and un-vetted. The UI
  must make the boundary unmistakable; promotion gating (`promotion_record`
  severity rules) must remain the trust boundary for *acting on* content.
- **Grant lifecycle.** Plan-scoped grants need real expiry/revocation (infra
  exists). Envelope expansion must revoke + reissue; emergency stop and plan
  withdraw must revoke immediately.
- **Drift while pending (case-study failure #2).** Pillar B reduces the
  frequency of pending plans but does not by itself prevent lateral drift when
  one *is* pending. Decide whether a pending plan should hard-block non-plan
  tool calls, or whether that is too restrictive. Left as an open question (§9).
- **Poll-loop guidance (case-study failure #3).** The `workflow_wait` poll loop
  violates Ri-0.14. This is a guidance/mechanical-guard issue, tracked
  separately (§7.5), but it interacts with legibility: the tiered timeline must
  surface "⚠ poll loop" as a checkpoint so the operator sees it instantly.

## 9. Open questions

1. Should a *pending* plan hard-block lateral agent actions (spawns/tests), or
   merely block promotion? (Case study says the current middle ground — block
   promotion only — produces drift.)
2. Should plan-scoped grants be visible/revocable through the existing
   `gateway grants revoke` CLI, or do they need a plan-scoped surface?
3. How is draft lineage pruned to keep the content tree bounded in long
   sessions? (Cap blobs-per-name; expire old pointers?)
4. Is the structural plan diff authoritative for `requires_regate`, or does the
   operator get an override ("force re-approve")?

## 10. Build order

Each slice is independently shippable.

| # | Slice | Touches | Risk |
|---|-------|---------|------|
| 1 | **Timeline tiers + checkpoint elevation** | `cli/room/tui.rs`, `cli/room/render.rs` (view only) | low — pure view, reuses existing squash |
| 2 | **Plan diff + inherit-by-default** | `runtime/tools/plan_frame.rs`, new `plan_frame_diff.rs`, checkpoint rendering | medium — touches the gate; start strict |
| 3 | **t=0 session content tree pane** | new TUI pane over existing content store + name registry | medium — no new storage semantics |
| 4 | **Plan-as-capability-grant** | wire plan approval → grant materialization → 5th dedup layer | high — touches dedup core; do last |

(1) and (2) dissolve most of the case-study frustration and are safe. (3)
delivers the realtime-product visibility. (4) is the structural approval win.

## 11. Non-goals

- Changing the artifact immutability model. Blobs stay content-addressed and
  immutable; only *visibility* of current pointers changes.
- Delegating approval decisions to the agent (P-2.20/P-2.21 agent-as-decider).
  This design keeps the operator gate first-class and makes the envelope
  machine-checkable.
- The poll-loop / guidance drift (case-study failure #3). Tracked separately —
  see `docs/AGENTS.md` "Coordinating With Children" and Ri-0.14. The timeline
  surface here merely makes such drift *visible*.
