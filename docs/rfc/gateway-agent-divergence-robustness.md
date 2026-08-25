# RFC: Gateway Robustness Against Agent Divergence

**Status:** Draft — 2026-06-24 (rev. 2 — corrected after code audit + author review).
Implementation plan in §7.

> **Rev. 2 corrections (2026-06-24).** A code audit found three load-bearing
> claims in rev. 1 were wrong, and the RFC author concurred. The fixes are
> folded in throughout; the headline changes:
> 1. **D.2 rested on `ToolErrorType` variants that do not exist**
>    (`CapabilityMissing`, `SandboxUnavailable`, `ApprovalRequired`,
>    `ContinuationIntegrity`). D.2 is rewritten to classify from the *real*
>    surface — the 9 existing variants + the `≥128` exit-code path + the
>    `[sandbox_driver_unavailable]` marker.
> 2. **The Sentinel never blocks execution** — verified, across the board, not
>    just the `user_interaction` path. The lived complaint is therefore
>    *mislabelling* (the causal-event + operator-message stream calls healthy
>    work "diverging"), not blocking. The blocker is the LoopGuard. Sequencing
>    is re-weighted toward the changes that touch the LoopGuard (D.2/D.4/E).
> 3. **The approval/clarification operator-tedium is already shipped**
>    (the two sibling plans are now archived; live behaviour in
>    `docs/wiki/approval-system.md`). This RFC no longer claims to address it
>    except indirectly via Change A. See §6.

**Origin:** Recurring patch pattern. ≈537 `fix`-prefixed commits in the last six
months, ≈111 in the last two weeks. Three of the most recent
(`4629447c`, `e316cd53`, `48747332`) instantiate the *same* "Lawful-Executor"
pattern — verify agent self-report against observable state instead of trusting
it. Each was patched as a point-fix. Meanwhile the divergence sentinel
repeatedly mislabels legitimate repair loops as divergence
(`bf50ea1c` is the documented instance) because its signals measure *counts*,
not *trajectories of error→fix*.

**Related:**
- `docs/design/divergence-sentinel-design.md` — the original Sentinel design (P0 done, P1 done, P2 done)
- `docs/rfc/unit-test-runner-divergence-loop.md` — the "category error" diagnosis this RFC generalizes
- `docs/reports/postmortems/session-b6d27af2-weather-agent.md` — attribution: "mostly agents not following their own prompts"
- `docs/concepts/gateway-architecture-principles.md` — "Dumb Gateway, Smart Agent" tenet
- `docs/reports/2026-04-24-constitution-audit.md` — §5 flags the egress-validation gap
- `docs/archived/approval-system-hardening-plan.md`, `docs/archived/human-gate-unification-plan.md` — the *shipped* approval/clarification hardening (see §6)
- `autonoetic-gateway/src/runtime/guard.rs`, `trajectory_monitor.rs`, `trajectory_health.rs`, `response_validation.rs`

---

## 1. Problem

Two intertwined failures keep producing patches.

### 1.1 The gateway treats each fabrication as a point bug

The codebase already has the right instinct — "verify the agent's self-report
against observable state" — but it is applied ad hoc. The bulk of response
validation is centralized (policy-driven `validate_spawn_response` in
`response_validation.rs`, ~12 `rule:` strings covering size/schema/artifact
policy). The *ad-hoc seam* is narrower but real: the **self-report claim
guards** — `delegated_without_spawn`, `unknown_plan_id`, `promotion_record_*`
— are each a separate hand-written check bolted into `validate_and_maybe_repair`,
added reactively after an incident:

| Incident | Fabrication | Patch |
|---|---|---|
| Session-1344bdd3 | Planner self-approved its own plan via `PlanFrameAccess: ["*"]` | `e316cd53` |
| Session-14722070 | Planner invented `plan_id: "plan-a1b2c3d4"` with no `planframe_propose` call | `4629447c` |
| Earlier | Agent ended with `status: "delegated"` and never spawned a child | `48747332` |

Each new shortcut the LLM invents = one new hand-written claim guard + tests +
a patch. There is no general "self-report ↔ observable-state reconciliation"
primitive, so coverage of *this class* is discoverable only by incident.

### 1.2 The divergence sentinel cannot tell repair from divergence — and mislabels the difference

The user's lived complaint: *"the divergence sentinel tends to block processes
due to LLM looping with bad schema or misusing tools, but it is hard to know
what is a normal loop of repair or an LLM becoming dumb."*

Two things are true at once, and rev. 1 conflated them:

- **Mechanically, the Sentinel does not block.** It is observational only; the
  **LoopGuard** is the sole component that trips and halts a session (on its
  mechanical budgets). Verified across `trajectory_monitor.rs`,
  `trajectory_health.rs`, `lifecycle.rs`.
- **Experientially, the operator reads a Sentinel `Critical` as "it killed my
  session."** The Sentinel's `Critical` verdict fires just before (or alongside)
  a LoopGuard trip and floods the causal-event stream + operator message with a
  *divergence* label. So the pollution is **mislabelling**: healthy work and
  gateway-side blocks get stamped "diverging" in the audit trail and the
  operator notification, even though the Sentinel didn't pull the trigger.

The most acute form of this mislabelling is on the **TUI**: a `Critical` verdict
(with `notify_operator=true`, the default) calls `create_user_interaction` with
`kind: DivergenceSentinel`, rendering a `🔔` card with a signal dump and the
prompt *"💬 Type your answer below"* — options `[Acknowledge, Stop]` plus a
freeform note field. It is non-blocking, but it *looks* like a clarification the
operator must answer, and the operator cannot: *Acknowledge* is a no-op and
*Stop* is an emergency halt.
This is a notification wearing a clarification's clothes — a `UserInteraction`
(a **decision atom**, per `docs/archived/human-gate-unification-plan.md`) used to
deliver what is really passive **information**. D.7 addresses the surface; D.2
and D.6 reduce how often it fires.

This is a **category error**, the same shape as the one diagnosed in
`docs/rfc/unit-test-runner-divergence-loop.md` (`ok:false` conflating
sandbox-malfunction with domain-failure), but at the loop level. Every signal
the Sentinel computes today is **count-based over a window**:

| Signal | Definition | File |
|---|---|---|
| `loop_pressure` | `current_loops / max_loops_without_progress` | `trajectory_health.rs:302` |
| `failure_pressure` | `max tool failures / max_tool_failures` | `trajectory_health.rs:315` |
| `repetition_entropy` | Shannon entropy of fingerprints | `trajectory_monitor.rs` |
| `error_burst` | error events in last-N turns | `trajectory_monitor.rs` |

Counts cannot distinguish three situations that look identical from the
counting layer:

| Situation | What is happening | Is it divergence? | Current handling |
|---|---|---|---|
| **A. Productive repair** | LLM emits bad schema → gets a structured violation → fixes one field → fixes another → succeeds. Each iteration has a *different* error. | No — the repair loop (`validate_and_maybe_repair`) is doing its job. | LoopPressure + FailurePressure climb; Sentinel labels **Diverging/Critical**; LoopGuard may trip. |
| **B. Stuck on irrecoverable error** | Permission/quota denied, sandbox driver unavailable. The LLM *cannot* fix it by retrying. | No — it is a blocked state, not dumbness. | Same counts as A/C; same mislabel; LoopGuard may trip on the failure budget. |
| **C. Genuine divergence** | LLM ignores feedback, fabricates, cycles a roster without reading results, retries the exact doomed command. | Yes. | Same counts/label as A/B — indistinguishable. |

Because A and B accumulate the same counts as C, the Sentinel **mislabels** them
identically, spams the operator, trains operators to ignore escalations, and —
where the same counts also exhaust a LoopGuard budget — the LoopGuard trips on
healthy work. The one documented fix (`bf50ea1c` — demote `RepetitionEntropy`
to advisory after `researcher.default` false-gated on turn 1) is a per-signal
patch on a structural problem.

---

## 2. Root cause

### 2.1 Counts measure effort, not outcome

`max_loops_without_progress` and `max_tool_failures` answer *"how hard is the
agent trying?"* They were designed as **mechanical budgets** — circuit
breakers to prevent unbounded resource burn. That job is correct and the
LoopGuard should keep doing it.

The Sentinel then *derives* its verdicts from those same counts
(`signals_from_loop_guard` in `trajectory_health.rs:286`). That coupling
overloads the budgets with a second job they cannot perform: judging whether
the effort is productive. A budget cannot tell you that; only the **shape of
the error trajectory** can.

### 2.2 Feedback is given but never tracked

When the gateway repairs a response, it already emits a structured violation
and rebuilds the prompt (`response_validation.rs`, `validate_and_maybe_repair`
in `execution.rs`). When a tool fails, it returns a typed error
(`ToolErrorType`) with a repair hint. **The gateway emits a lot of feedback.
It does not record whether the next turn's behavior changed in response.**

That gap is exactly what separates A from C:
- **A (repair):** feedback → different behavior → different (or no) error.
- **C (divergence):** feedback → same behavior → same error.

The single highest-signal indicator of "LLM becoming dumb" is *repeating the
same error after being told*. No current signal measures it.

### 2.3 Irrecoverable failures are counted as dumbness

`guard.rs` already exempts `ToolErrorType::Permission` from the failure budget
(`register_failure` early-returns on `Permission`) — the agent cannot fix it by
retrying. But the exemption is narrow, and rev. 1 of this RFC proposed widening
it to four *non-existent* variants. The real surface is nine variants
(`Validation, Permission, Resource, Execution, Fatal, Conflict, QuotaExceeded,
NotFound, Timeout`) plus two non-error mechanisms:

| Condition | How it is surfaced today | "Retry can't help"? |
|---|---|---|
| Lacking permission / capability | `ToolErrorType::Permission` (capability-missing rides on Permission) | **Yes** — already exempt |
| Quota / budget exhausted | `ToolErrorType::QuotaExceeded` | **Yes** — *not* exempt today |
| Sandbox driver unavailable (`1a7274b4`) | `ToolErrorType::Resource` + `[sandbox_driver_unavailable]` message marker | **Yes** — *not* exempt today |
| Signal-derived exit (`≥128`) | `ok:false` on the tool result (set at the `(0..128)` check), **not** an `error_type` | **Yes** — *not* exempt today |
| Transient timeout | `ToolErrorType::Timeout` | **No** — often transient; a retry can help |
| Approval required | `YieldReason::ApprovalRequired` — a **suspension**, never a `register_failure` call | n/a — never reaches the failure counter |
| Continuation integrity failure | internal `CheckpointIntegrityError` — tampered checkpoints are **silently skipped**, agent never sees it | n/a — never reaches the failure counter |

The first four genuinely feed divergence pressure and should be exempted. The
last two are **not tool errors and never increment the failure counter**, so
they need no exemption — rev. 1 was wrong to list them. The Sentinel therefore
penalizes the agent for the gateway's own blocks (`QuotaExceeded`,
sandbox-unavailable, `≥128`) — but the fix is to read the *real* surface, not to
invent a taxonomy.

### 2.4 The self-report gap feeds the Sentinel noise

When the LLM fabricates success (`delegated` without spawn, `plan_id` without
propose, `promotion_record` without evidence), the Sentinel sees a clean
*ok:true* progress fingerprint and counts the turn as healthy. The lie is
discovered one or two turns later when downstream tooling rejects the
nonexistent reference — by which point the agent has "progressed" past the
point where a clean rollback is possible, and the resulting flailing looks
like divergence *of the agent's making*. Catching fabrication earlier (the
self-report primitive in Change A) removes a whole noise source from the
Sentinel's input.

---

## 3. Proposed changes

The changes split into two families:

- **Stop the actual blocking** (LoopGuard-facing): **D.2, D.4, E**. These reduce
  real LoopGuard trips on healthy work — the only thing that mechanically halts
  a session.
- **Stop the mislabelling** (Sentinel-facing): **D.1, D.3, D.6**. These fix the
  causal-event + operator-message stream so healthy work and gateway-side blocks
  stop being stamped "diverging."

**A, B, C** are enabling cleanups that reduce the noise both families have to
classify.

### Change A — Self-report reconciliation primitive

**Priority:** P1 — high leverage for the *fabrication* class; converts future
fabrications from "write a new hand-written claim guard + tests + ship a patch"
to "add an enum variant".
**Files:** `autonoetic-gateway/src/runtime/response_validation.rs` (lift the
hand-written claim guards into a typed surface), `execution.rs`
(`validate_and_maybe_repair`).

Generalize the pattern `delegated_without_spawn` and `unknown_plan_id` already
instantiate. Today each claim guard is a hand-written check inside
`validate_and_maybe_repair`; instead, make the set of claimable fields
**typed and closed**:

```rust
enum ClaimKind {
    Delegated,          // status == "delegated" → a child TaskRun must exist
    PlanId,             // plan_id mentioned   → a PlanFrame must exist
    PromotionVerdict,   // promotion_record    → bound evaluator/auditor trace must exist
    ArtifactBuilt,      // artifact_ref cited  → artifact store must contain it
    CapabilityEnvelope, // declared NetworkAccess hosts → must match detected URLs
}

struct Claim {
    kind: ClaimKind,
    field_path: &'static str,   // where in the reply the claim lives
    verify: fn(&ClaimCtx) -> ClaimVerdict,
}

enum ClaimVerdict { Ok, Unverified, Fabricated(String) }
```

Each `Claim` carries its own verifier; the response validator walks the reply,
extracts every claim it finds, and reconciles each against causal state. The
property this buys is **mechanical**: "every claimable field has a verifier"
becomes statically checkable (a single match arm per `ClaimKind`), and a new
fabrication mode is one enum variant instead of a new hand-written guard.

The capability-wildcard rule from `e316cd53` also folds in here: authority
operations (`planframe.approve`, and every equivalent authority-class
operation) are *never* satisfied by `*` or prefix patterns — only by exact
grants. Generalize the #602 fix so the rule is stated once for all authority
ops, not rediscovered per tool.

**Why this reduces Sentinel noise:** fabricated success currently looks
healthy for 1–2 turns and then collapses into disorderly flailing. Early
detection turns that into a single clean repair iteration (Situation A),
which the classifier then correctly labels.

### Change B — Decouple tool-success from domain-success everywhere

**Priority:** P1 — closes the category error the unit-test-runner RFC
diagnosed, beyond the one tool it landed for.
**Files:** `autonoetic-gateway/src/runtime/tools/sandbox.rs:2085`,
`lifecycle.rs:~3095` (LoopGuard + trajectory keying on `ok`).

**Already landed for `artifact_exec`:** the unit-test-runner fix shipped the
two-field result there — `artifact_exec.rs` reports both `ok` (sandbox ran the
command to completion without a sandbox-level malfunction) and
`command_succeeded` (the command's own exit status), with `ok=false` for exit
`≥128`. The remaining work is to **extend the same pattern to `sandbox.rs`**
(which still reports only `ok`, derived from `output.status.success()`) and to
ensure `LoopGuard::register_failure` and the trajectory observation's `failed`
flag key on `ok` only — the domain result is the agent's to interpret.

Signal-derived exit codes (`≥128`) stay `ok:false` per the existing rule —
those are genuine sandbox faults, not domain results (and they feed the
`is_irrecoverable` path in D.2 via the `ok` flag, not via `error_type`).

### Change C — Egress validation (close the one-way perimeter)

**Priority:** P2 — flagged as MISSING in
`docs/reports/2026-04-24-constitution-audit.md:116`.
**Files:** `autonoetic-gateway/src/runtime/lifecycle.rs` (child→parent result
injection), `response_validation.rs` (reuse the `io.returns` validator).

Confirmed asymmetry: child output **is** validated against the child's
`io.returns` (`validate_and_maybe_repair` runs on the `SpawnResult` before it
returns to the parent), but messages *into* child sessions are **not** validated
against `io.accepts`, and there is no claim-level reconciliation on the
child→parent path. This is the asymmetry that let the fabricated `plan_id`
propagate until a downstream tool rejected it. Closing the perimeter means
validating child→parent results against the same self-report claims in Change A
(the `io.returns` schema check already exists). It removes another upstream
noise source from the classifier. **Advisory first** (log mismatches without
gating) until the corpus test (§5.3) confirms low false-positive rate.

### Change D — Feedback-aware classification + blocked-state separation

**Priority:** the core of the RFC, split across the two families above.
**Files:** `autonoetic-gateway/src/runtime/trajectory_monitor.rs`,
`trajectory_health.rs` (new signals + revised aggregation), `guard.rs`
(`register_failure` irrecoverable exemption + `is_irrecoverable`), `execution.rs`
(feedback-event recording).

#### D.2 Separate "blocked" from "divergent" — *(LoopGuard-facing; lead change)*

Introduce a **blocked-state** classification distinct from divergence, and —
crucially — exempt irrecoverable failures from the **LoopGuard failure budget**
(`register_failure`), not just from the Sentinel signal. This is the change that
stops the LoopGuard tripping on the gateway's own blocks.

Generalize the existing `Permission` early-return in `register_failure` into a
typed predicate built from the **real** surface (§2.3), *not* invented variants:

```rust
fn is_irrecoverable(e: &ToolErrorType) -> bool {
    matches!(
        e,
        ToolErrorType::Permission        // lacks right/capability — retry can't help (already exempt)
        | ToolErrorType::QuotaExceeded   // budget/quota gone — retry can't refill mid-loop
        | ToolErrorType::SandboxUnavailable  // NEW variant — see note
    )
    // NOTE: Timeout is NOT here — often transient, a retry can help.
    // NOTE: generic Resource is NOT here — only the sandbox-driver-unavailable
    //       case is irrecoverable, which the new variant captures cleanly.
}
// Plus, at the tool-result layer (not error_type): exit_code >= 128 (ok=false,
// signal-derived) is treated as irrecoverable for failure-budget purposes.
```

**Decision (enum surface).** Promote **`SandboxUnavailable`** to a first-class
`ToolErrorType` variant. Today the condition rides on `Resource` + a
`[sandbox_driver_unavailable]` string marker (landed in `1a7274b4`); a typed
predicate must not string-sniff a marker to decide an irrecoverable exemption,
and exempting *all* `Resource` errors would wrongly exempt genuine
agent-caused resource exhaustion. The marker stays as a transition aid; the
variant is authoritative. `CapabilityMissing` is **not** added — capability
gaps already surface as `Permission`, which is already exempt, so a split is
cosmetic for this purpose (track separately as optional type-hygiene).
`ApprovalRequired` and `ContinuationIntegrity` are **not** added — they are a
`YieldReason` and an internal `CheckpointIntegrityError` respectively, neither
of which ever calls `register_failure`, so they need no exemption.

Irrecoverable failures:

- Do **not** increment the LoopGuard failure budget (`register_failure` returns
  early) and do **not** feed `failure_pressure` or `error_burst`.
- Do feed a separate `blocked_state` signal whose only action is operator
  notification with a clear "the agent is blocked, not diverging" message.

This removes Situation B from both the divergence label *and* the failure
budget — most of the current false-positive load.

#### D.4 Repair-loop-aware accounting — *(LoopGuard-facing)*

When `validate_and_maybe_repair` is actively cycling (`repair_rounds > 0`,
bounded by `output_policy.declared_repair_attempts()` / `max_repair_rounds`),
switch to repair-aware accounting:

- Repair iterations count against the repair loop's own declared budget,
  **not** against `max_loops_without_progress`.
- Escalation within a repair loop fires only on **repeated identical
  violations** (D.1), never on growing-distinct violations.
- A successful repair resets the repair-window accounting.

This stops the double-counting where one repair cycle burns both the repair
budget *and* `max_loops_without_progress` — a real LoopGuard trip on healthy
work.

#### E (cross-ref) Role-aware budgets — *(LoopGuard-facing)*

See Change E below. Mis-tuned budgets are the third real cause of LoopGuard
trips on healthy work; E is grouped with D.2/D.4 for sequencing even though it
lives in config.

#### D.1 Track feedback events and their incorporation — *(Sentinel-facing)*

Record a `FeedbackEvent` every time the gateway tells the agent something it
should act on: a response-validation violation, a typed tool error carrying a
repair hint, an `approval_ref` injection. Each event carries the violation's
signature (rule id + normalized field path) or the error's signature
(tool + `ToolErrorType` + normalized message).

On the next turn, compute a `feedback_incorporated` fraction over the window:

| Window observation | Classification |
|---|---|
| Same violation reappears after feedback | **Feedback ignored** → strong divergence signal |
| Different violation each iteration, none repeating | **Productive repair** → suppress escalation |
| Distinct error signatures growing | **Productive struggle** → suppress escalation |
| One error signature repeating with no behavior change | **Stuck or divergent** → see D.2 |

This single signal does what no count can: it distinguishes Situation A from
Situation C directly. A repair loop where each iteration's error is *different*
is healthy by construction; a loop where the *same* error repeats verbatim is
the real "LLM becoming dumb."

#### D.3 Pin the "Sentinel never blocks" invariant — *(Sentinel-facing; cheap)*

The Sentinel is **already** observational-only across all paths (verified, not
just the `user_interaction` path) — the LoopGuard is the sole blocker. Rev. 1
wrongly implied the Sentinel blocks; the real defect is that its `Critical`
verdict floods the audit/operator stream alongside a LoopGuard trip, so it
*reads* as the killer. The work here is therefore to **document this as an
invariant and pin it with a test**, not to change behaviour:

| Layer | Job | May block execution? |
|---|---|---|
| **LoopGuard** | Hard mechanical budget — prevent unbounded resource burn | **Yes.** Unchanged. |
| **Sentinel** | Judge whether budget consumption is productive | **No.** Observational only. |

A regression test asserts that no Sentinel verdict (including `Critical`) raises
an execution-blocking gate; only the LoopGuard trips.

#### D.5 Suppress-on-progress grace — *(Sentinel-facing)*

If the last N turns show feedback being incorporated (violations addressed,
error signatures evolving, distinct successful tool calls increasing),
suppress all Sentinel escalation for the next M turns. Productive struggle
buys grace. This is the general form of the `researcher.default` warm-up fix
in `bf50ea1c`, applied continuously rather than just at session start.

#### D.6 Revised aggregation: confirmed repetition is the only gate-worthy signal — *(Sentinel-facing)*

Following the precedent `bf50ea1c` set for `RepetitionEntropy`, generalize:
the only signal that may justify an operator-blocking escalation is **confirmed
repeated identical failure after feedback was given**. Everything else
(`LoopPressure`, `FailurePressure`, `ErrorBurst`, `ContextPressure`) stays
advisory — logged, surfaced in the digest, messaged to the planner, but
non-blocking. (Note: per D.3 the Sentinel never blocks directly regardless;
this rule governs which verdict it is *allowed to label* `Critical`.)

The aggregation rule in `trajectory_health.rs:219` becomes:

1. `blocked_state` confirmed → `Blocked` (operator notification, non-blocking).
2. `feedback_ignored` confirmed (same signature post-feedback) → `Diverging`
   (planner message) → repeated → `Critical` (operator notification justified).
3. Otherwise → at most `Watching` (advisory log only).

#### D.7 Divergence escalation is an advisory, never a clarification — *(Sentinel-facing)*

This addresses the operator's lived symptom directly: divergence currently
surfaces as a `UserInteraction` popup the operator cannot meaningfully answer
(§1.2). The fix is the **surface**, not just the frequency.

**Files:** `autonoetic-gateway/src/runtime/lifecycle.rs`
(`build_critical_divergence_interaction` / the `notify_operator` path),
`autonoetic-gateway/src/runtime/operator_activity.rs`, `autonoetic/src/cli/chat.rs`
(rendering), planner SKILL doctrine.

The governing principle is already in the codebase
(`docs/archived/human-gate-unification-plan.md`): a `UserInteraction` is a
**decision atom** — reserved for the case where the agent *literally cannot
proceed without a binary answer*. Divergence is **information**: the session is
not suspended (the LoopGuard is the only hard stop), so there is no decision the
operator must make for the agent to continue. Expressing it as a
`UserInteraction` is therefore a category error.

The change splits into two halves with different risk profiles.

**D.7a — Gateway surface swap (Sentinel-facing; Low risk).**

1. **Stop creating a `DivergenceSentinel` `UserInteraction`.** Route the
   `Critical` operator surface to a **passive advisory** instead — an
   `operator_activity` feed entry + a dismissable TUI banner. It carries the
   same signal evidence, but demands no answer and costs the operator nothing
   to ignore. (`operator_alert` audit logging stays as-is.)
2. **No `Acknowledge`/`Stop` prompt** (the popup today also carries a freeform
   note field — drop all of it). Those are the worst pair: a no-op and a nuclear
   option. If the operator wants to act, they invoke an **operator-initiated**
   intervention affordance (a TUI keybinding that opens the divergence detail +
   the same Stop/continue actions) — pulled, not pushed.
3. **Interactive gates only for genuine decisions.** A `UserInteraction` /
   `GateKind::UserInput` may still be raised when there is a real,
   outcome-changing choice the operator can make (e.g. an agent's own
   `user_ask`). Divergence is not such a case.

This is a deterministic surface swap across three touch-points
(`lifecycle.rs` creation, `operator_activity.rs` feed, `chat.rs` rendering)
with no model-behaviour dependency — genuinely low risk, pin with a test.

**D.7b — Planner doctrine (behaviour-change; Medium risk — needs an eval).**

A `[Sentinel Notice]` agent-message must prompt the planner to **self-correct
or replan**, not to bounce the decision to the operator via `user_ask`. Adjust
planner SKILL guidance so a divergence notice does not become a second operator
clarification. **This is a real behaviour change, not a code swap:** a planner
that still bounces Sentinel notices to `user_ask` defeats D.7a entirely, so it
needs its own validation (a small eval over divergence-notice prompts
confirming the planner self-corrects rather than asks the operator). Treat it
as the riskier half and gate it on that eval.

**Why this is the decisive fix for the lived complaint:** D.2 and D.6 make the
escalation *rare*; D.7 makes it *non-intrusive when it does happen*. Together
they convert "a popup I can't answer" into "an advisory I can ignore or act on
at will." Note that `notify_operator=false` is **not** the answer — that would
lose the signal entirely; the goal is to keep the signal and fix its surface.

### Change E — Role-aware budgets

**Priority:** P1 — grouped with D.2/D.4 because it is the third real cause of
LoopGuard trips on healthy work (the unit-test-runner RFC's Change 2 patched
exactly this, for one agent).
**Files:** `autonoetic-types/src/config.rs` (`LoopGuardConfig`),
`agents/*/SKILL.md` (declared budget profiles).

The reason `unit_test_runner` kept dying was a budget tuned for *reasoning*
agents applied to a *deterministic test executor*. The fix (raising
`max_tool_failures` to 4) was a per-manifest patch. Generalize: budget
profiles should be **role-aware**, derived from the agent's declared execution
shape (`execution_mode`, declared tool surface, typical workflow length)
rather than from a global default. A reasoning agent that should converge in
5 turns and a test runner that legitimately takes 8 turns with failing suites
need different budgets — and the agent's manifest is where that intent is
already declared.

---

## 4. Priority and sequencing

Sequencing is re-weighted (rev. 2) toward the **LoopGuard-facing** changes,
because those are the only ones that stop the *actual* blocking the operator
feels; the Sentinel-facing changes stop the *mislabelling*.

| # | Change | Family | Priority | Risk | Depends on |
|---|---|---|---|---|---|
| D.2 | Blocked vs divergent + `is_irrecoverable` exemption in `register_failure` | LoopGuard | **P0** | Low — extends the existing Permission early-return; one new enum variant | — |
| D.4 | Repair-loop-aware accounting | LoopGuard | **P0** | Med — touches `validate_and_maybe_repair` ↔ LoopGuard accounting | D.1 (signatures) |
| E | Role-aware budgets | LoopGuard | P1 | Low — config surface change | — |
| D.1 | Feedback-event tracking + incorporation signal | Sentinel | P0 | Low — additive; new signal | B (clean `ok` semantics) |
| D.6 | Aggregation: only confirmed repetition is gate-worthy | Sentinel | P0 | Low — rule change in `aggregate` | D.1, D.2 |
| D.3 | Pin "Sentinel never blocks" invariant (test only) | Sentinel | P0 | Trivial — already true; document + test | — |
| D.7a | Divergence surface swap (advisory, not a `UserInteraction` popup) | Sentinel | **P0** | Low — deterministic surface swap; pin with test | — |
| D.7b | Planner doctrine: a Sentinel notice → self-correct, not `user_ask` | Sentinel | P1 | **Med — behaviour change; needs an eval** | D.7a |
| D.5 | Suppress-on-progress grace | Sentinel | P1 | Low — additive suppression | D.1 |
| A | Self-report reconciliation primitive | Cleanup | P1 | Med — lift hand-written claim guards into typed surface | — |
| B | Extend `ok`/`command_succeeded` to `sandbox.rs` | Cleanup | P1 | Low — `artifact_exec` pattern already landed | — |
| C | Egress validation (advisory first) | Cleanup | P2 | Med — new validation surface | A (reuse claim verifiers) |

**The honest minimum, if only one change lands, is D.2.** It is the lowest-risk
change that touches the actual blocker: extending `is_irrecoverable` in
`register_failure` immediately stops the most demoralizing real failure — the
LoopGuard tripping (and the Sentinel mislabelling) when the agent is stuck on
the gateway's own permission/quota/sandbox block.

**Dependency-free first relief is D.2 + E** (Phase 1) — neither depends on
another change, and together they remove most real LoopGuard trips on healthy
work.

**The full de-pollution set is the whole table, but sequenced, not
simultaneous,** because of the dependency column:

- *Stop the blocking:* D.2 + E first; **D.4 comes later** — it needs D.1's
  feedback signatures, so it cannot sit in a true Phase 1 despite being
  blocking-relief. (This is the one item to keep out of any "minimum" claim.)
- *Stop the mislabelling:* D.1 + D.6 + D.3 + D.7a; D.7b follows once its planner
  eval passes.

A, B, C reduce upstream noise and make the classifier's job easier, but are not
part of the minimum.

---

## 5. Test plan

### 5.1 Unit tests

- `ClaimKind` reconciliation: each variant returns `Fabricated` when the
  observable state is absent, `Ok` when present, `Unverified` when the field
  is absent from the reply.
- `is_irrecoverable`: returns true for `Permission`, `QuotaExceeded`,
  `SandboxUnavailable`, and (via the result layer) the `≥128` exit-code family;
  returns false for `Timeout` and generic `Resource`. Assert `ApprovalRequired`
  / continuation-integrity paths never reach `register_failure`.
- `register_failure`: an irrecoverable error does **not** increment the failure
  budget; a recoverable one does.
- `feedback_incorporated`: same-signature-repeat after a feedback event → 0.0;
  distinct-signature sequence → high fraction.
- `aggregate` revised rule: `blocked_state` alone → `Blocked`;
  `feedback_ignored` confirmed → `Diverging` then `Critical`;
  lone `LoopPressure` warn → `Watching` only.

### 5.2 Integration tests

- Agent hitting permission/quota/sandbox-unavailable in a loop → `Blocked`
  event, **no** divergence label, LoopGuard does **not** trip on the failure
  budget, session not killed.
- Agent repairing a schema violation across 3 iterations, each error
  different → no divergence label; repair succeeds; LoopGuard does not
  double-count against `max_loops_without_progress`.
- Agent repeating the *same* violation after a feedback event → `Diverging`
  then `Critical`; operator notified.
- Sentinel `Critical` on a non-repetition signal (e.g. `LoopPressure` alone)
  → logged + planner messaged, **no** execution-blocking gate raised (D.3 invariant).
- Fabricated `plan_id` reconciliation → single repair iteration (Change A),
  no disorderly flailing downstream.

### 5.3 Falsifiable corpus test (the real acceptance criterion)

Mirror the methodology in `docs/design/divergence-sentinel-design.md` §6.
Collect 20 archived sessions: 10 productive-repair sessions that ultimately
succeeded, 10 operator-flagged genuine divergences. Strip outcomes, run the
new classifier blind.

**Success criteria:**

- Productive-repair sessions are **never** labelled `Critical` nor tripped by
  the LoopGuard. (Target: 0/10 false escalations. Today this fails.)
- Genuine divergence is caught **no later** than the current Sentinel, ideally
  earlier because feedback-ignored fires before the LoopGuard budget exhausts.
- Operator notifications on the productive-repair set drop to near-zero.

If the corpus test fails, D is not ready and the advisory-only default
(D.6) stays; the Sentinel keeps observing but its `Critical` label stays
non-actionable until tuning lands.

---

## 6. Scope: what this RFC does *not* address

**Approval / clarification operator-tedium is out of scope — and already
shipped.** Rev. 1 implied this RFC would relieve the "approvals and
clarifications make sessions tedious" pain and punted it to two sibling plans.
Those plans are now **archived and substantially implemented**:

- `docs/archived/approval-system-hardening-plan.md` — grant scope/target-kinds/
  expiry, revocation, analytics, HMAC checkpoint integrity (two residual issues:
  #606, #607).
- `docs/archived/human-gate-unification-plan.md` — unified `GateService`, the
  redundant-approval and credential bugs fixed, clarification child sessions.
- Live operational reference: `docs/wiki/approval-system.md`.

This RFC touches that pain **only indirectly**, via **Change A**: catching
fabrication before it triggers a downstream gate removes a class of spurious
approval/clarification prompts. It makes no other claim on approval UX.

**One exception, in scope: D.7.** The *divergence* escalation popup is not part
of the approval/clarification gate system — it is the Sentinel *misusing* the
`UserInteraction` primitive to deliver a non-decision (§1.2). D.7 removes that
misuse. It does not touch the GateService, agent `user_ask`, or any genuine
operator decision gate — those are correct and shipped.

Also out of scope:

- **Layer 2 LLM watchdog** (`docs/design/divergence-sentinel-design.md` §4).
  The deterministic classifier (Change D) must be validated first.
- **Cross-session divergence memory.** Sentinel judgments stay within the
  session causal chain; learning across sessions is the curator's job.
- **Redesigning the LoopGuard as a semantic-progress oracle.** The LoopGuard
  stays a mechanical budget. Change D makes the Sentinel *interpret* its
  outputs rather than derive from them directly.

---

## 7. Implementation plan

Not started. Each phase below is a self-contained, shippable PR with its own
tests; phases are ordered so earlier ones do not depend on later ones. Per-phase
tracking issues should link back to this section.

### Phase 1 — Stop the actual blocking (LoopGuard) · D.2 + E

Highest relief, lowest risk. No Sentinel changes required.

1. Add `ToolErrorType::SandboxUnavailable`; map the `[sandbox_driver_unavailable]`
   path (`sandbox.rs`, `artifact_exec.rs`) to it (keep the marker as a transition
   aid). *(`autonoetic-types/src/tool_error.rs`, sandbox drivers.)*
2. Add `is_irrecoverable(&ToolErrorType) -> bool` and call it in
   `register_failure` (`guard.rs`) so `Permission | QuotaExceeded |
   SandboxUnavailable` and `≥128`-exit results skip the failure budget.
3. Emit a `blocked_state` signal + operator notification ("blocked, not
   diverging") when an irrecoverable error is hit.
4. Role-aware budgets (Change E): derive `LoopGuardConfig` from the manifest's
   declared execution shape; default profile unchanged.
5. Tests: §5.1 `is_irrecoverable` / `register_failure`, §5.2 blocked-loop case.

**Exit criterion:** a permission/quota/sandbox-unavailable loop no longer trips
the LoopGuard and is labelled `Blocked`, not `Diverging`.

### Phase 2 — Stop the mislabelling (Sentinel) · D.1 + D.6 + D.3 + D.7a (D.7b gated on eval)

1. `FeedbackEvent` recording + signatures in `execution.rs`; `feedback_incorporated`
   signal in `trajectory_health.rs`. *(Depends on Phase 3's `ok` semantics only
   where tool-error signatures are involved; recordable independently.)*
2. Revise `aggregate` (`trajectory_health.rs:219`) to the three-rule form (D.6),
   consuming `blocked_state` (Phase 1) and `feedback_ignored`.
3. Pin the "Sentinel never blocks" invariant with a regression test (D.3).
4. **D.7a (gateway surface):** replace the `DivergenceSentinel` `UserInteraction`
   with a passive `operator_activity` advisory + dismissable TUI banner; remove
   the `Acknowledge`/`Stop` prompt (and its freeform note) in favour of an
   operator-initiated intervention keybinding. Deterministic; pin with a test.
5. **D.7b (planner doctrine — gated on its own eval):** adjust planner SKILL
   guidance so a `[Sentinel Notice]` triggers self-correction/replan, not a
   `user_ask` bounce. Ship only once a small divergence-notice eval confirms the
   planner self-corrects rather than asking the operator (a planner that still
   bounces defeats D.7a).
6. Tests: §5.1 `feedback_incorporated` / `aggregate`, §5.2 same-violation and
   non-repetition cases; assert no `UserInteraction` of `kind:
   DivergenceSentinel` is created on `Critical` (D.7a); planner-doctrine eval (D.7b).

**Exit criterion:** distinct-error repair loops are never labelled `Critical`;
only confirmed post-feedback repetition is; and no divergence verdict pushes an
answer-demanding popup at the operator.

### Phase 3 — Reduce upstream noise · B + A

1. **B:** extend the landed `artifact_exec` two-field result (`ok` /
   `command_succeeded`) to `sandbox.rs`; key `register_failure` and the
   trajectory `failed` flag on `ok` only.
2. **A:** lift the hand-written claim guards into the `ClaimKind`/`Claim`
   typed surface; fold the `e316cd53` authority-wildcard rule into the
   authority-op class.
3. Tests: §5.1 `ClaimKind` reconciliation; §5.2 fabricated-`plan_id` case.

### Phase 4 — Polish · D.4 + D.5 + C

1. **D.4:** repair-loop-aware accounting (needs Phase 2 signatures).
2. **D.5:** suppress-on-progress grace.
3. **C:** egress validation, **advisory first** (reuses Change A verifiers).

### Phase 5 — Acceptance · §5.3 corpus test

Build the 20-session blind corpus and gate the move from advisory to actionable
on the success criteria. If it fails, D.6 advisory-only stays.

### Cross-references for issue authors

- `is_irrecoverable` / `register_failure` exemption: `guard.rs` (the existing
  `Permission` early-return is the template).
- Aggregation: `trajectory_health.rs:219` (`aggregate`), signals at `:286`
  (`signals_from_loop_guard`), `:302` (`loop_pressure`), `:315` (`failure_pressure`).
- Two-field tool result precedent: `artifact_exec.rs` (`ok` + `command_succeeded`,
  `(0..128)` check); the gap is `sandbox.rs:2085`.
- Repair budget: `output_policy.declared_repair_attempts()` /
  `max_repair_rounds` in `response_validation.rs`.
