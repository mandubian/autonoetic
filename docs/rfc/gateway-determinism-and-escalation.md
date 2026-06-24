# RFC: Mechanical Determinism and Context-Rich Escalation

**Status:** Draft — 2026-06-24. Feedback wanted before implementation.
Implementation plan in §6.

**Thesis.** The gateway should handle LLM nondeterminism *mechanically* — with
closed, typed rules — not with a growing pile of reactive point-patches added
each time a model does something unexpected, and not with operator escalations
that ask for a decision without supplying the context to make one. This RFC
audits where the gateway has drifted from that, against three principles, and
proposes the principled replacements.

**The three principles (the design law this RFC enforces):**

1. **Recoverable error → bounded retry, *unless an output is possible*.** A
   recoverable failure may be retried, but the bound must be principled:
   progress toward a result earns more attempts; flailing does not.
2. **Non-recoverable error → stop.** Then, depending on criticality, escalate —
   but **only with enough analysis/context to actually decide.** *An
   approval/escalation without a strong explanation is useless.*
3. **Async child that loops while its parent moves on → must be governed.** A
   child whose result no one will consume is ungoverned waste; abandonment must
   be a deliberate, recorded, resource-reclaiming act.

**Founding invariant (the deepest of the three — Principle 2 is its corollary):
the gateway enables choices; it never makes them.** Every gate *output* — every
point where the gateway *asks* a decider for a decision or *exposes* state a
decider will act on — carries a typed `DecisionContext` sufficient to choose
correctly. This is universal, not selective: it holds for every `GateKind`
(`Approval`, `UserInput`, `Escalation`) and every decider. Two consequences
that this RFC treats as law:

- **Decider symmetry.** In Autonoetic, *all* deciders — human operators and
  agents alike — are governed by one rights/rules framework; they differ only in
  **authority and voting weight** (a democratic model that does not exist yet but
  is the design's destination). Context is therefore owed to the *decider role*,
  not to "the operator": a human and an agent resolving the same gate receive the
  **same** `DecisionContext`. (This is the proactive complement of P-2.21, which
  already says an agent-decider with *insufficient context* must escalate rather
  than guess — here the gateway's job is to make sure that never happens by
  supplying the context up front.)
- **The gateway never decides on a decider's behalf.** Its obligation is to
  deliver the data that makes the right choice *possible*, never to substitute
  its own judgment for the decider's. (Where the gateway *must* exercise reserved
  judgment, that is a tracked **DISCRETION LEAK**, not a feature.) This is the
  mirror of the decider's own duty under **O-1** (a decision owes a motivation):
  O-1 binds the decider to give a reason; this invariant binds the gateway to
  give the context.

**Related:**
- `docs/rfc/gateway-agent-divergence-robustness.md` — the Sentinel/LoopGuard
  rework. **Overlap is deliberate and bounded:** that RFC owns *retry-bound
  progress-awareness* (its D.4) and the *blocked-vs-divergent* classification;
  this RFC owns *error-class unification*, *escalation context quality*,
  *async-child governance*, and *reactive-patch consolidation*. Cross-references
  are marked inline.
- `docs/gateway-architecture-principles.md` — "Dumb Gateway, Smart Agent" /
  Lawful Executor.
- `docs/archived/human-gate-unification-plan.md` — the shipped `GateService`;
  the gate-context standard in §3.2 extends it.
- Code anchors are cited per finding (verified 2026-06-24).

---

## 1. Problem

Two drifts, both away from the Lawful-Executor ideal:

### 1.1 The gateway has become a *forgiving parser* in places

Each time a model emits malformed output or misuses a tool, the response has
often been a point-patch: tolerate *this* malformation, alias *this* shorthand,
strip *this* vendor token. Individually reasonable; in aggregate they teach the
model it can cut corners and the gateway will guess. A sample of the live
patches (verified):

| Patch | Reacts to | Anchor |
|---|---|---|
| `approval_ref` injected into tool-call JSON + prose hint | model ignores text-only approval hints, retries without the ref | `execution.rs:35-83` |
| Markdown-fence stripping (reply + plan_id) | models wrap JSON in ` ```json ` fences | `response_validation.rs:663-712`; planner path (`a74e9fc7`) |
| Tool-name aliasing (`spawn`→`agent_spawn`, `fetch`→`web_fetch`, …) | models use shorthands | `tool_call_processor.rs:63-71` |
| Agent-called-as-tool hint | models confuse agents with tools | `tool_call_processor.rs:469-478` |
| `command`/`cmd`/`script` field try-chain | inconsistent arg field names | `tool_call_processor.rs:779-794` |
| Gemma special-token stripping | vendor leaks `<|"\|>` into args | `tool_call_processor.rs:48-60` |
| Multi-field success inference (`command_succeeded`/`ok`/`exit_code`) | heterogeneous tool-result success markers | `tool_call_processor.rs:689-718` |
| Fuzzy find-and-replace | `content_patch` models can't emit exact substrings | `fuzzy_match.rs` |
| Lenient `Option<String>` coercion | models send numbers/bools for strings | `tools/mod.rs:783` |

Not all of these are wrong (fuzzy patch is genuinely useful). The defect is the
*absence of a doctrine*: which tolerances are legitimate closed normalizations
vs. which hide a contract the gateway should enforce — and the fact that none
are surfaced as "the model cut a corner here" for learning.

### 1.2 Escalations ask without explaining

The `GateService` is shipped and correct in mechanism, but several gates surface
to the operator with **no rationale** — the "useless approval" anti-pattern.
Verified bare summaries:

| Gate | Operator sees (`summary`) | `reason` | Anchor |
|---|---|---|---|
| `web.search` | `web.search {host}` | `web.search to {host} requires approval` | `web.rs:897-912` |
| `web.fetch` | `web.fetch {host}` | `…requires approval` | `web.rs:1298-1308` |
| `web.call` | `web.call {host}` | `…requires approval` | `web.rs:1911-1926` |
| `credential_request` | `Credential request to {host}` | `HTTP request to {host} requires approval` | `credential.rs:505-520` |
| `credential_setup` (fetch) | `Fetch skill.md from {host}` | — | `credential.rs:1651-1668` |
| `credential_setup` (API) | `Credential setup API call to {host}` | — | `credential.rs:1943-1960` |

The `reason` is circular ("approval is required because approval is required").
None state the **query/payload**, the **agent's intent**, **what secret is at
stake**, or a **recommended action**. The operator cannot assess risk.

**The capability to do this right already exists.** `sandbox.exec` /
`artifact_exec` build a rich `operator_reason` — command + intent + network
analysis + detected code patterns (`sandbox.rs:1370`, `artifact_exec.rs:552`).
The web/credential gates simply don't. (Note: `user_ask` is **fine** — its
`reason`/`summary` are boilerplate, but the real question flows through
`GateKind::UserInput.question` and is what the TUI renders; `user_interaction.rs:238`.)

### 1.3 Classification is split and partly stringly-typed

Error handling crosses three representations: `ToolErrorType` (typed, 9
variants, clean `is_recoverable()` at `tool_error.rs:318`), `FailureClass`
(workflow-level), and `RetryAdvice`. The `ToolErrorType → FailureClass` mapping
is done by **string-matching the message** (`failure_classification.rs:195-248`,
~30 `.contains("timed out")`-style checks) — fragile and duplicative of the
typed information already on the error.

### 1.4 Abandoned async children are not reclaimed

A *looping* child already self-stops (its own LoopGuard trips → turn fails). The
gap is **abandonment**: when a parent proceeds without a child (parallel-join
timeout, parent completes, or root budget exhausts), the child keeps running as
a background task until the R+12 reaper notices (~30 s+), and the reaper only
updates the DB row — it does **not** abort the task handle (`scheduler.rs:706-849`).
Only emergency-stop truly kills (`abort` + SIGKILL). Worse, **root-budget
exhaustion does not cascade**: `root_session_budget.rs` blocks the parent's next
LLM call but has no path to cancel in-flight descendants, so they keep burning
the tree budget that is already blown.

---

## 2. What is already right (keep / use as the model)

To avoid the divergence RFC's rev-1 mistake of "fixing" things that work:

- **Non-recoverable stop is correct.** `tool_call_processor.rs:202-215`
  explicitly checks `!is_recoverable()` and hard-returns `Err("Fatal tool
  error…")`. Principle 2's "stop" half is implemented; this RFC does **not**
  touch it.
- **Scheduler retry is the model.** `evaluate_stage_retry()`
  (`workflow_store.rs:65`) is recoverability-aware, side-effect-aware
  (`SideEffectState::Committed` → do not retry), and escalates on `Unknown`.
  Other retry sites should converge toward this shape.
- **`sandbox.exec` escalation context** is the bar for §3.2.
- **A looping child self-stops** via its own LoopGuard — no change needed for
  that case.

---

## 3. Proposed changes

### Change E1 — Universal `DecisionContext` on every gate *(the Founding invariant; highest user-felt)*

**Files:** `human_gate.rs` (the `GateRequest`/`GateResult` contract — the
single chokepoint), then every gate-producing site: `web.rs`, `credential.rs`,
`sandbox.rs`/`artifact_exec.rs`, `user_interaction.rs`, `session.rs`,
`agent_revision.rs`, the planner/plan-approval path, and any future gate.

**This is the universal form of the Founding invariant, not a web/credential
patch.** A `DecisionContext` is **mandatory and typed on every gate output** —
every `GateKind` (`Approval`, `UserInput`, `Escalation`), to every decider
(human or agent). Nothing is *asked* (a gate raised) or *exposed* (state surfaced
for a decision) without it. The enforcement point is structural: `GateRequest`
carries a required `DecisionContext` value (not a free-form string), so a gate
with no real context **fails to construct** rather than reaching a decider —
mechanical, not reviewer-discretion.

A `DecisionContext` always answers four questions, with depth graduated by
stakes (the tiers are *floors*, applied to every gate, never an opt-in):

- **What** — the concrete action + target (never a bare method name + host).
- **Why gated** — the policy/rule that forced the gate, in prose, with its ID.
- **What is at stake** — which secret / payload / query / capability / budget,
  and the reversibility and blast radius.
- **Recommended action + how to decide** — what the gateway would expect a
  correct decider to weigh; for an agent-decider, what would make it escalate.

Stake tiers (floor depth, applied universally):

- **Tier 1 (self-explanatory):** What + Why gated. (e.g. a profile read.)
- **Tier 2 (network/credential/API):** + intent + at-stake + recommended action.
- **Tier 3 (code execution / elevated authority / irreversible):** Tier 2 +
  analysis + detected patterns (the existing `sandbox.exec` `operator_reason` is
  the bar — generalize it as the `DecisionContext` builder, not a one-off).

**Decider symmetry is part of the contract:** the same `DecisionContext` is
delivered whether the gate is resolved by a human operator (TUI/CLI), an
agent-decider (`P-2.20`/`P-2.21`, the same `approve_request`/`reject_request`
API), or a future policy/voting body. The gateway renders the *same* data; only
authority/voting weight differs across deciders.

**Why first:** it is the most direct relief for the "approvals are useless /
tedious" complaint, it is low-risk (additive context, no control-flow change),
and — because it is enforced at the `GateRequest` chokepoint — it closes the
class permanently: no future gate can regress to a thin one. The known thin
offenders (`web.*`, `credential.*`) are simply the first migrations; the
chokepoint is what makes it universal.

### Change R1 — One typed error classification *(Principle 1)*

**Files:** `failure_classification.rs`, `tool_error.rs`, callers.

Replace the message-string heuristics with a single typed function
`classify(&ToolError) -> FailureClass` driven by `ToolErrorType` (+ the
sandbox-unavailable marker and `≥128` exit handling that the divergence RFC's
D.2 introduces — reuse, don't duplicate). Keep string-matching **only** as a
logged last-resort fallback for genuinely untyped legacy errors, so its use is
visible and shrinking. This makes recoverability and retry-class a property of
the *type*, not of prose.

> **Cross-ref (not in this RFC):** progress-aware retry *bounds* (retry more
> when output is possible) are the divergence RFC's **D.4** + feedback signal.
> R1 gives D.4 a clean typed input; the bound logic stays there.

### Change C1 — Graceful child abandonment *(Principle 3)*

**Files:** `scheduler.rs` (R+12 reaper), `active_execution_registry.rs`,
`lifecycle.rs` / `workflow.rs` (join paths).

When a parent stops needing a child (join timeout, parent terminal, explicit
"continue without"), the child must be **checkpointed, then cancelled, with a
recorded reason** — not left to a 30 s reaper that doesn't abort the task. The
reaper (and the abandonment path) must call `abort_workflow_tasks` (the same
mechanism emergency-stop already uses, `active_execution_registry.rs:86-97`),
not merely update the DB row. The checkpoint preserves forkability/audit; the
recorded reason answers "why was this child abandoned?".

**Design stance (the open question, resolved):** abandon = **stop**, gracefully.
A child whose result no parent will read is ungoverned waste and a budget drain.
The only exception is the existing, correct one: a child **parked at an approval
gate** is *suspended*, not abandoned, and must not be reaped (`scheduler.rs:711-717`).

### Change C2 — Budget exhaustion cascades to descendants *(Principle 3)*

**Files:** `root_session_budget.rs`, `execution.rs` (budget-exceeded path).

Root-session-tree budget exhaustion (P-7.10) must **cancel in-flight
descendants**, not just block the parent's next LLM call. Today the tree budget
can be blown while children keep spending it. On budget-exceeded, run the same
graceful-cancel path as C1 over the descendant set. (This is a straightforward
correctness fix — arguably a bug against P-7.10.)

### Change M1 — Reactive-patch doctrine + consolidation *(cross-cutting)*

**Files:** `tool_call_processor.rs`, `response_validation.rs`, LLM driver layer.

Establish a doctrine and reclassify every tolerance in §1.1:

- **Legitimate closed normalization — keep, but centralize + log.** Fuzzy patch,
  lenient string coercion: useful; keep, but emit a one-line "normalized model
  output (kind=…)" so corner-cutting is *visible*, not silent.
- **Vendor quirk — move to the driver boundary.** Gemma token stripping belongs
  in the LLM driver, not scattered through tool-call processing.
- **Format leniency — one chokepoint.** Markdown-fence stripping should be a
  single shared step, not re-implemented per call site.
- **Missing contract — enforce + repair-hint.** Tool-name aliasing and the
  `command`/`cmd`/`script` try-chain hide an unstable tool API. Pick the
  canonical form, reject the rest with a typed repair hint (and a deprecation
  window that logs the alias before removing it), so the model learns.

The test of M1: a new model quirk should be absorbed by an *existing* normalizer
(closed set) or rejected with a hint — never by adding a new bespoke branch.

---

## 4. Priority and sequencing

| # | Change | Principle | Priority | Risk |
|---|---|---|---|---|
| E1 | Universal `DecisionContext` on every gate (chokepoint + migrate all sites) | Founding invariant | **P0** | Low — additive context; required typed `DecisionContext` at the `GateRequest` chokepoint |
| C2 | Budget exhaustion cascades to descendants | 3 | **P0** | Low — correctness fix; reuse emergency-stop cancel |
| R1 | One typed error classification | 1 | P1 | Low–Med — replace string heuristics; fallback logged |
| C1 | Graceful child abandonment (reaper aborts handle) | 3 | P1 | Med — touches reaper + join paths; checkpoint-then-cancel |
| M1 | Reactive-patch doctrine + consolidation | x-cut | P2 | Med — per-patch; some are API changes with deprecation windows |

**If only one lands first: E1.** It is the most direct answer to the lived
"approvals are useless" pain and carries no control-flow risk.

---

## 5. Test plan

- **E1:** every gate of **any** `GateKind` carries a non-boilerplate
  `DecisionContext` (what + why-gated + at-stake + recommended action); a unit
  test asserts no gate is constructed with empty/boilerplate context, and a
  decider-symmetry test asserts a human and an agent-decider receive the same
  context for the same gate.
- **R1:** `classify()` returns the correct `FailureClass` per `ToolErrorType`
  with no message string needed; the string fallback is hit only for untyped
  legacy errors and logs when it is.
- **C1:** a parent that abandons a still-running child → child is checkpointed,
  its task handle aborted, a `child_abandoned` reason recorded; no orphan task
  survives past the abandonment point.
- **C2:** root-budget exhaustion with two in-flight children → both are
  cancelled; tree spend stops climbing after the exhaustion event.
- **M1:** a synthetic new model quirk is either normalized by an existing closed
  normalizer (logged) or rejected with a typed repair hint — no new bespoke
  branch required to handle it.
- **Regression:** the non-recoverable stop (`tool_call_processor.rs:202`) and the
  scheduler retry decision tree are unchanged (pin with existing tests).

---

## 6. Implementation plan

Each phase is a self-contained PR with its own tests; phases are independent
unless noted. Per-phase tracking issues mirror the changes and link to the
umbrella.

### Phase 1 — Universal `DecisionContext` (E1)
- Add a required typed `DecisionContext` to `GateRequest` covering **all**
  `GateKind`s (`Approval`, `UserInput`, `Escalation`) in `human_gate.rs`; a gate
  with empty/boilerplate context **fails to construct** (the chokepoint that
  makes the invariant universal and regression-proof).
- Generalize the existing `sandbox.exec` `operator_reason` builder into the
  shared Tier-3 `DecisionContext` builder.
- Migrate every gate-producing site to supply it — first the known thin
  offenders (`web.*`, `credential.*`), then the rest (`user_interaction.rs`,
  `session.rs`, `agent_revision.rs`, plan-approval).
- Decider-symmetry test: the same `DecisionContext` is delivered to a human
  decider and an agent-decider (`P-2.20`/`P-2.21`) for the same gate.
- Tests: no gate of **any** kind constructs with boilerplate context; render
  shows What / Why-gated / At-stake / Recommended-action. **Exit:** no gate,
  to any decider, is asked or exposed without sufficient context to decide.

### Phase 2 — Budget cascade (C2)
- On root-budget exhaustion, cancel in-flight descendants via the graceful-cancel
  path (`root_session_budget.rs` + `execution.rs`).
- Tests: C2 above. **Exit:** tree spend cannot climb after exhaustion.

### Phase 3 — Typed error classification (R1)
- `classify(&ToolError) -> FailureClass` from `ToolErrorType` (+ reuse D.2's
  SandboxUnavailable / ≥128); demote string-matching to a logged fallback.
- Tests: R1 above. **Exit:** classification needs no message string for typed errors.

### Phase 4 — Graceful child abandonment (C1)
- Reaper + join paths checkpoint-then-`abort_workflow_tasks`; record
  `child_abandoned` reason; preserve the approval-gate-parked exception.
- Tests: C1 above. **Exit:** no orphan task survives abandonment; ~30 s zombie
  window closed.

### Phase 5 — Reactive-patch consolidation (M1)
- Apply the doctrine: centralize+log normalizations, move Gemma stripping to the
  driver, single fence chokepoint, canonicalize tool-name/field aliases with a
  logged deprecation window.
- Tests: M1 above. **Exit:** new quirks are absorbed by closed normalizers or
  rejected with hints, not new branches.

### Out of scope
- Retry-bound progress-awareness (owned by divergence RFC D.4 / #612).
- Blocked-vs-divergent classification and the Sentinel surface (divergence RFC).
- Redesigning the `GateService` decider model (shipped; see human-gate plan).
