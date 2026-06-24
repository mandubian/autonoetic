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
2. **Non-recoverable error → stop.** Then, depending on criticality, escalate to
   the operator/authority — but **only with enough analysis/context to actually
   decide.** *An approval/escalation without a strong explanation is useless.*
3. **Async child that loops while its parent moves on → must be governed.** A
   child whose result no one will consume is ungoverned waste; abandonment must
   be a deliberate, recorded, resource-reclaiming act.

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

### Change E1 — Escalation context standard *(Principle 2; highest user-felt)*

**Files:** `human_gate.rs` (`GateRequest` contract), `web.rs`, `credential.rs`.

Make a **decision context mandatory and typed** on every operator-facing gate.
A gate may not be created with a boilerplate rationale. Define a tiered standard:

- **Tier 1 (self-explanatory):** action + concrete target. (e.g. a profile read.)
- **Tier 2 (network/credential/API):** action + target + **intent** (what the
  agent is trying to do) + **what is at stake** (which secret / which payload /
  which query) + **why gated** (the policy, in prose) + **recommended action**.
- **Tier 3 (code execution):** Tier 2 + analysis + detected patterns (the
  existing `sandbox.exec` `operator_reason`).

Apply Tier 2 to all `web.*` and `credential.*` gates (bring them to the
`sandbox.exec` bar). Enforce mechanically: `GateKind::Approval` carries a
required `DecisionContext` struct (not a free-form string), so a thin gate fails
to compile rather than reaching the operator.

**Why first:** this is the most direct relief for the lived "approvals are
useless / tedious" complaint, and it is low-risk (additive context, no
control-flow change).

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
| E1 | Escalation context standard (web + credential to Tier 2) | 2 | **P0** | Low — additive context; typed `DecisionContext` |
| C2 | Budget exhaustion cascades to descendants | 3 | **P0** | Low — correctness fix; reuse emergency-stop cancel |
| R1 | One typed error classification | 1 | P1 | Low–Med — replace string heuristics; fallback logged |
| C1 | Graceful child abandonment (reaper aborts handle) | 3 | P1 | Med — touches reaper + join paths; checkpoint-then-cancel |
| M1 | Reactive-patch doctrine + consolidation | x-cut | P2 | Med — per-patch; some are API changes with deprecation windows |

**If only one lands first: E1.** It is the most direct answer to the lived
"approvals are useless" pain and carries no control-flow risk.

---

## 5. Test plan

- **E1:** every `web.*` / `credential.*` gate carries a non-boilerplate
  `DecisionContext` (intent + stake + policy + recommended action); a unit test
  asserts no gate is constructed with an empty/boilerplate context.
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

### Phase 1 — Escalation context standard (E1)
- Add a typed `DecisionContext` to `GateRequest`/`GateKind::Approval`
  (`human_gate.rs`); reject empty/boilerplate at construction.
- Populate Tier 2 context for every `web.*` gate (`web.rs`) and `credential.*`
  gate (`credential.rs`): intent, stake, policy-in-prose, recommended action.
- Tests: no gate with boilerplate context; operator-facing render shows the new
  fields. **Exit:** web/credential approvals are decidable without external context.

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
