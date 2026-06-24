# RFC: Gateway Robustness Against Agent Divergence

**Status:** Draft — 2026-06-24. Feedback wanted before implementation.
**Origin:** Recurring patch pattern. 823 `fix` commits in 6 months, 132 in the
last two weeks alone. Three of the most recent patches
(`4629447c`, `e316cd53`, `48747332`) instantiate the *same* "Lawful-Executor"
pattern — verify agent self-report against observable state instead of trusting
it. Each was patched as a point-fix. Meanwhile the divergence sentinel
repeatedly over-fires on legitimate repair loops
(`bf50ea1c` is the documented instance) because its signals measure *counts*,
not *trajectories of error→fix*.
**Related:**
- `docs/design/divergence-sentinel-design.md` — the original Sentinel design (P0 done, P1 done, P2 done)
- `docs/rfc/unit-test-runner-divergence-loop.md` — the "category error" diagnosis this RFC generalizes
- `docs/postmortems/session-b6d27af2-weather-agent.md` — attribution: "mostly agents not following their own prompts"
- `docs/gateway-architecture-principles.md` — "Dumb Gateway, Smart Agent" tenet
- `docs/gateway-constitution-audit-2026-04-24.md` — §5 flags the egress-validation gap
- `autonoetic-gateway/src/runtime/guard.rs`, `trajectory_monitor.rs`, `trajectory_health.rs`, `response_validation.rs`

---

## 1. Problem

Two intertwined failures keep producing patches:

### 1.1 The gateway treats each fabrication as a point bug

The codebase already has the right instinct — "verify the agent's self-report
against observable state" — but it is applied ad hoc. Today
`response_validation.rs` carries 33 distinct `rule:` strings
(`delegated_without_spawn`, `unknown_plan_id`, `promotion_record_*`,
`artifact_build_evidence`, …), each added reactively after an incident:

| Incident | Fabrication | Patch |
|---|---|---|
| Session-1344bdd3 | Planner self-approved its own plan via `PlanFrameAccess: ["*"]` | `e316cd53` |
| Session-14722070 | Planner invented `plan_id: "plan-a1b2c3d4"` with no `planframe_propose` call | `4629447c` |
| Earlier | Agent ended with `status: "delegated"` and never spawned a child | `48747332` |

Each new shortcut the LLM invents = one new validator + tests + a patch. There
is no general "self-report ↔ observable-state reconciliation" primitive, so
coverage is *discoverable only by incident*.

### 1.2 The divergence sentinel cannot tell repair from divergence

The user's lived complaint: *"the divergence sentinel tends to block processes
due to LLM looping with bad schema or misusing tools, but it is hard to know
what is a normal loop of repair or an LLM becoming dumb."*

This is a **second category error** — the same shape as the one diagnosed in
`docs/rfc/unit-test-runner-divergence-loop.md` (`ok:false` conflating
sandbox-malfunction with domain-failure), but at the loop level. Every signal
the Sentinel computes today is **count-based over a window**:

| Signal | Definition | File |
|---|---|---|
| `loop_pressure` | `current_loops / max_loops_without_progress` | `trajectory_health.rs:302` |
| `failure_pressure` | `max_tool failures / max_tool_failures` | `trajectory_health.rs:315` |
| `repetition_entropy` | Shannon entropy of fingerprints | `trajectory_monitor.rs` |
| `error_burst` | error events in last-N turns | `trajectory_monitor.rs` |

Counts cannot distinguish three situations that look identical from the
counting layer:

| Situation | What is happening | Is it divergence? | Current Sentinel verdict |
|---|---|---|---|
| **A. Productive repair** | LLM emits bad schema → gets a structured violation → fixes one field → fixes another → succeeds. Each iteration has a *different* error. | No — the repair loop (`validate_and_maybe_repair`) is doing its job. | **Fires.** LoopPressure + FailurePressure both climb. |
| **B. Stuck on irrecoverable error** | Permission denied, capability missing, sandbox driver unavailable, continuation integrity failure. The LLM *cannot* fix it by retrying. | No — it is a blocked state, not dumbness. | **Fires.** Same counts as A/C. |
| **C. Genuine divergence** | LLM ignores feedback, fabricates, cycles through a roster without reading results, retries the exact doomed command. | Yes. | **Fires.** Indistinguishable from A/B. |

Because A and B fire the same way as C, the Sentinel pollutes: it blocks
healthy work, spams the operator, and trains operators to ignore its
escalations. The one documented fix (`bf50ea1c` — demote `RepetitionEntropy`
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

`guard.rs:285` already exempts `ToolErrorType::Permission` from the failure
budget — the agent cannot fix it by retrying. But the exemption is narrow:
`CapabilityMissing`, `SandboxUnavailable` (the subject of `1a7274b4`),
`ApprovalRequired`, `ContinuationIntegrity`, and the `signal-derived` exit
codes (≥128, per the unit-test-runner RFC) all share the property *"retrying
cannot help"* and all still feed divergence pressure. The Sentinel therefore
penalizes the agent for the gateway's own blocks.

### 2.4 The self-report gap feeds the Sentinel noise

When the LLM fabricates success (`delegated` without spawn, `plan_id` without
propose, `promotion_record` without evidence), the Sentinel sees a clean
*ok:true* progress fingerprint and counts the turn as healthy. The lie is
discovered one or two turns later when downstream tooling rejects the
nonexistent reference — by which point the agent has "progressed" past the
point where a clean rollback is possible, and the resulting flailing looks
like divergence *of the agent's making*. Catching fabrication earlier (the
self-report primitive in §3.1) removes a whole noise source from the
Sentinel's input.

---

## 3. Proposed changes

The changes are grouped: **A–C** are enabling cleanups that reduce the noise
the Sentinel has to classify; **D** is the Sentinel rework itself; **E** is
budget calibration. The Sentinel rework depends on A and C being at least
partially in place.

### Change A — Self-report reconciliation primitive

**Priority:** P0 — highest leverage; converts future fabrications from
"write a new validator + tests + ship a patch" to "add an enum variant".
**Files:** `autonoetic-gateway/src/runtime/response_validation.rs` (refactor
the existing 33 rules into a typed surface), `execution.rs`
(`validate_and_maybe_repair`).

Generalize the pattern `delegated_without_spawn` and `unknown_plan_id` already
instantiate. Today every claimable field is a hand-written check; instead,
make the set of claimable fields **typed and closed**:

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
fabrication mode is one enum variant instead of a new module section.

The capability-wildcard rule from `e316cd53` also folds in here: authority
operations (`planframe.approve`, and every equivalent authority-class
operation) are *never* satisfied by `*` or prefix patterns — only by exact
grants. Generalize the #602 fix so the rule is stated once for all authority
ops, not rediscovered per tool.

**Why this reduces Sentinel noise:** fabricated success currently looks
healthy for 1–2 turns and then collapses into disorderly flailing. Early
detection turns that into a single clean repair iteration (Situation A),
which the Sentinel then correctly classifies.

### Change B — Decouple tool-success from domain-success everywhere

**Priority:** P1 — closes the category error the unit-test-runner RFC
diagnosed, beyond the one tool it landed for.
**Files:** `autonoetic-gateway/src/runtime/tools/sandbox.rs:2085`,
`artifact_exec.rs`, `lifecycle.rs:3091-3189` (LoopGuard + trajectory paths).

The unit-test-runner RFC (§6, "out of scope") explicitly deferred the same
decoupling for `sandbox_exec`. Revisit it now, in general form: every tool
that executes a command should report **two fields**, `ok` (the tool worked:
the sandbox ran the command to completion without a sandbox-level
malfunction) and `command_succeeded` (the command's own exit status). The
LoopGuard's `register_failure` and the trajectory observation's `failed` flag
should key on `ok` only; the domain result is the agent's to interpret.

Signal-derived exit codes (≥128) stay `ok:false` per the existing rule —
those are genuine sandbox faults, not domain results.

### Change C — Egress validation (close the one-way perimeter)

**Priority:** P1 — flagged as MISSING in
`docs/gateway-constitution-audit-2026-04-24.md:116`.
**Files:** `autonoetic-gateway/src/runtime/lifecycle.rs` (child→parent result
injection), `response_validation.rs` (reuse the `io.returns` validator).

Today, ingress (messages into child sessions) is schema-validated; tool
*results* flowing back to parents are not. This is the asymmetry that let the
fabricated `plan_id` propagate until a downstream tool rejected it. Closing
the perimeter means validating child→parent results against the child's
declared `io.returns` and against the same self-report claims in Change A.
It removes another upstream noise source from the Sentinel.

### Change D — Feedback-aware trajectory classification (the Sentinel rework)

**Priority:** P0 for the "not polluting" goal. This is the core of the RFC.
**Files:** `autonoetic-gateway/src/runtime/trajectory_monitor.rs`,
`trajectory_health.rs` (new signals + revised aggregation), `guard.rs`
(irrecoverable-failure exemption list), `execution.rs`
(feedback-event recording).

#### D.1 Track feedback events and their incorporation

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

#### D.2 Separate "blocked" from "divergent"

Introduce a **blocked-state** classification distinct from divergence. An
agent that has hit a mechanically irrecoverable error (permission, capability
missing, sandbox unavailable, continuation integrity, repeated signal-derived
exit ≥128) is *blocked*, not divergent. The right action is to surface the
blockage to the operator (or auto-degrade), not to imply the LLM is dumb.

Generalize the existing `ToolErrorType::Permission` exemption in `guard.rs:285`
into a typed `is_irrecoverable(error_type) -> bool` predicate covering the
full set. Irrecoverable failures:

- Do **not** feed `failure_pressure` or `error_burst`.
- Do feed a separate `blocked_state` signal whose only action is operator
  notification with a clear "the agent is blocked, not diverging" message.

This removes Situation B from the Sentinel's divergence bucket entirely —
which is most of the current false-positive load.

#### D.3 The Sentinel must never block; it may only escalate

Today the Sentinel itself does not kill sessions — the LoopGuard does, and
the Sentinel derives its verdicts from LoopGuard pressure, so `Critical`
fires just before the trip and *operationally* feels like the Sentinel killed
it. Make the separation explicit and durable:

| Layer | Job | May block execution? |
|---|---|---|
| **LoopGuard** | Hard mechanical budget — prevent unbounded resource burn | **Yes.** Unchanged. |
| **Sentinel** | Judge whether budget consumption is productive | **No.** Observational only. |

Concretely: the Sentinel's `Critical` verdict continues to log, message the
planner, and notify the operator — but it must never itself raise a gate that
blocks execution. The only thing that blocks is the LoopGuard tripping on its
own mechanical budgets. (This is already true for the `user_interaction` path
after `bf50ea1c`; make it a documented invariant and pin it with a test.)

#### D.4 Repair-loop-aware mode

When `validate_and_maybe_repair` is actively cycling (`repair_rounds > 0`),
the Sentinel switches to repair-aware accounting:

- Repair iterations count against the repair loop's own declared budget
  (`output_policy.repair.max_attempts`), **not** against
  `max_loops_without_progress`.
- Escalation within a repair loop fires only on **repeated identical
  violations** (D.1), never on growing-distinct violations.
- A successful repair resets the repair-window accounting.

This stops the double-counting where one repair cycle burns both
`max_repair_attempts` *and* `max_loops_without_progress`.

#### D.5 Suppress-on-progress grace

If the last N turns show feedback being incorporated (violations addressed,
error signatures evolving, distinct successful tool calls increasing),
suppress all Sentinel escalation for the next M turns. Productive struggle
buys grace. This is the general form of the `researcher.default` warm-up fix
in `bf50ea1c`, applied continuously rather than just at session start.

#### D.6 Revised aggregation: confirmed repetition is the only gate-worthy signal

Following the precedent `bf50ea1c` set for `RepetitionEntropy`, generalize:
the only signal that may raise an operator-blocking gate is **confirmed
repeated identical failure after feedback was given**. Everything else
(`LoopPressure`, `FailurePressure`, `ErrorBurst`, `ContextPressure`) stays
advisory — logged, surfaced in the digest, messaged to the planner, but
non-blocking. The operator who wants to act on an advisory signal can; the
operator who ignores it does not lose work.

The aggregation rule in `trajectory_health.rs:219` becomes:

1. `blocked_state` confirmed → `Blocked` (operator notification, non-blocking).
2. `feedback_ignored` confirmed (same signature post-feedback) → `Diverging`
   (planner message) → repeated → `Critical` (operator gate is justified here).
3. Otherwise → at most `Watching` (advisory log only).

### Change E — Role-aware budgets

**Priority:** P2 — prevents the class of false-trip that the unit-test-runner
RFC's Change 2 patched for one agent.
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

| # | Change | Priority | Risk | Depends on |
|---|---|---|---|---|
| A | Self-report reconciliation primitive | P0 | Med — refactor of `response_validation.rs`; pure logic, well-tested | — |
| B | Decouple `ok` / `command_succeeded` everywhere | P1 | Low–Med — same shape as the landed `artifact_exec` change | — |
| C | Egress validation | P1 | Med — new validation surface; must be advisory first | A (reuse claim verifiers) |
| D.1 | Feedback-event tracking + incorporation signal | P0 | Low — additive; new signal | B (clean `ok` semantics) |
| D.2 | Blocked vs divergent classification | P0 | Low — generalizes existing Permission exemption | — |
| D.3 | Sentinel-never-blocks invariant (pin with test) | P0 | Trivial — already true post-`bf50ea1c`; just pin it | — |
| D.4 | Repair-loop-aware mode | P1 | Med — touches `validate_and_maybe_repair` accounting | D.1 |
| D.5 | Suppress-on-progress grace | P1 | Low — additive suppression | D.1 |
| D.6 | Aggregation: only confirmed repetition gates | P0 | Low — rule change in `aggregate` | D.1, D.2 |
| E | Role-aware budgets | P2 | Low — config surface change | — |

**Minimum viable non-polluting Sentinel:** D.1 + D.2 + D.3 + D.6. Those four
alone remove the bulk of false escalations by (a) classifying blocked as
non-divergent, (b) classifying productive repair as non-divergent, and (c)
pinning that the Sentinel never blocks. A, B, C reduce the upstream noise
further and make the classifier's job easier, but D is where the user's lived
complaint is answered.

If only one change lands first, it should be **D.2** (blocked vs divergent):
it is the lowest-risk, removes Situation B entirely, and immediately stops
the most demoralizing false-fire — the one where the agent is killed for the
gateway's own permission block.

---

## 5. Test plan

### 5.1 Unit tests

- `ClaimKind` reconciliation: each variant returns `Fabricated` when the
  observable state is absent, `Ok` when present, `Unverified` when the field
  is absent from the reply.
- `is_irrecoverable`: covers Permission, CapabilityMissing,
  SandboxUnavailable, ContinuityIntegrity, and the ≥128 exit-code family.
- `feedback_incorporated`: same-signature-repeat after a feedback event → 0.0;
  distinct-signature sequence → high fraction.
- `aggregate` revised rule: `blocked_state` alone → `Blocked`;
  `feedback_ignored` confirmed → `Diverging` then `Critical`;
  lone `LoopPressure` warn → `Watching` only (never escalates to a gate).

### 5.2 Integration tests

- Agent hitting permission-denied in a loop → `Blocked` event, **no**
  divergence escalation, session not killed by the Sentinel.
- Agent repairing a schema violation across 3 iterations, each error
  different → no divergence escalation; repair succeeds; LoopGuard does not
  double-count against `max_loops_without_progress`.
- Agent repeating the *same* violation after a feedback event → `Diverging`
  then `Critical`; operator gate raised.
- Sentinel `Critical` on a non-repetition signal (e.g. `LoopPressure` alone)
  → logged + planner messaged, **no** gate raised (D.3 invariant).
- Fabricated `plan_id` reconciliation → single repair iteration (Change A),
  no disorderly flailing downstream.

### 5.3 Falsifiable corpus test (the real acceptance criterion)

Mirror the methodology in `docs/design/divergence-sentinel-design.md` §6.
Collect 20 archived sessions: 10 productive-repair sessions that ultimately
succeeded, 10 operator-flagged genuine divergences. Strip outcomes, run the
new classifier blind.

**Success criteria:**

- Productive-repair sessions are **never** escalated to an operator gate.
  (Target: 0/10 false gates. Today this fails.)
- Genuine divergence is caught **no later** than the current Sentinel, ideally
  earlier because feedback-ignored fires before the LoopGuard budget exhausts.
- Operator notifications on the productive-repair set drop to near-zero.

If the corpus test fails, D is not ready and the advisory-only default
(D.6) stays; the Sentinel keeps observing but stops gating until tuning lands.

---

## 6. Out of scope

- **Layer 2 LLM watchdog** (`docs/design/divergence-sentinel-design.md` §4).
  The deterministic classifier (Change D) must be validated first; the
  watchdog experiment in §6 of the design doc is the gate for any LLM-based
  escalation.
- **Cross-session divergence memory.** Sentinel judgments stay within the
  session causal chain; learning across sessions is the curator's job.
- **Redesigning the LoopGuard as a semantic-progress oracle.** The LoopGuard
  stays a mechanical budget. Change D makes the Sentinel *interpret* its
  outputs rather than derive from them directly.
- **New scheduled-action variants or approval dedup layers** — those are
  covered by `docs/approval-system-hardening-plan.md` and
  `docs/design/human-gate-unification-plan.md`.

---

## 7. Implementation status

Not started. This RFC is the design reference; per-change phase issues should
mirror the sections above (A, B, C, D.1–D.6, E) and link back here.

### Indicative landing order

1. **D.2 + D.3** — immediate de-pollution (blocked ≠ divergent; pin
   never-blocks invariant). Lowest risk, highest relief.
2. **A** — self-report primitive. Unblocks C and reduces Sentinel input noise.
3. **D.1 + D.6** — feedback-aware classification + revised aggregation. The
   core of "robust but not polluting."
4. **B, C** — signal decoupling and egress validation. Reduce residual noise.
5. **D.4, D.5, E** — polish: repair-aware accounting, suppress-on-progress,
   role-aware budgets.
