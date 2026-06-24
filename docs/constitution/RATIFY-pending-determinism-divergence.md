# RATIFY.md — DRAFT for the Determinism + Divergence amendment (#621)

> **DRAFT — not yet a version.** Move this to
> `docs/constitution/versions/<APPLY_DATE>/RATIFY.md` when the version is cut
> (after divergence #610 merges and the operator runs `recompute_lock.py`).
> Companion to `docs/constitution/pending-amendment-determinism-divergence.md`,
> which holds the full clause edits + execution steps. Baseline: `2026.06.22`.

## Summary

Constitutional amendments for the gateway determinism + divergence-robustness
work (umbrellas #614 determinism, #608 divergence). Makes already-enforced
gateway behaviour lawful in signed text: irrecoverable-error handling, budget
cascade, graceful child abandonment, a new decider-context right, and (pending)
the feedback-aware non-progress signal and the Sentinel-never-blocks invariant.

**Prerequisites:** determinism clauses are READY (E1 #622, C2 #625, R1 #627,
C1 #628, M1 #629 all merged). Divergence clauses are PENDING **#610** (D.1/D.6
feedback, D.3 Sentinel-never-blocks) — apply them only once #610 merges and its
causal-event IDs are final. Signing requires `AUTONOETIC_CONSTITUTION_SIGNING_SK_B64`.

## Amendments

### P-6.21 — tree-budget cascade on exhaustion (amended) · READY (C2 / #625)

On root-tree budget exhaustion the gateway now cancels in-flight descendants
(graceful budget circuit breaker reusing the emergency-stop cascade, fired once
per root) so they cannot keep spending an exhausted tree budget.

**Diff:**
```
< | P-6.21 | Tree-wide budget aggregated across all descendants of a root session. | gateway-constitution-roadmap.md | `runtime/root_session_budget.rs` `runtime/lifecycle.rs:1254` | ENFORCED |
---
> | P-6.21 | Tree-wide budget aggregated across all descendants of a root session. On exhaustion the gateway cancels in-flight descendants (a graceful budget circuit breaker reusing the emergency-stop cascade), fired exactly once per root (idempotent), so descendants cannot keep spending an already-exhausted tree budget. | gateway-constitution-roadmap.md; this amendment | `runtime/root_session_budget.rs`, `execution.rs::trigger_root_budget_circuit_breaker`, `tests/root_budget_circuit_breaker.rs` | ENFORCED |
```
**Implementation:** PR #625.

### P-7.5 — irrecoverable errors do not count (amended) · READY (D.2 / #620)

The per-tool failure-budget exemption is generalized from `permission` to the
full irrecoverable set; irrecoverable conditions are surfaced as a blocked-state,
not divergence.

**Diff:**
```
< | P-7.5 | Loop guard trips on `max_tool_failures` per tool (configurable; current default in `docs/config-reference.md`); permission errors do not count. | ARCHITECTURE.md | `guard.rs::register_failure` + `check_loop` | ENFORCED |
---
> | P-7.5 | Loop guard trips on `max_tool_failures` per tool (configurable; current default in `docs/config-reference.md`); irrecoverable errors do not count — permission, quota_exceeded, sandbox_unavailable, and signal-derived exits (exit_code ≥ 128, `ok:false`). These are surfaced as a blocked-state (`operator_alert.blocked_state`), not divergence. | ARCHITECTURE.md; this amendment | `guard.rs::is_irrecoverable` + `register_failure` + `check_loop` | ENFORCED |
```
**Implementation:** divergence Phase 1 PR #620.

### P-7.16 — graceful child abandonment aborts live handles (amended) · READY (C1 / #628)

The orphan reaper now aborts the abandoned child's live execution handles (not
just the DB record), scoped to that child so live siblings are untouched;
approval-gate-parked children remain exempt.

**Diff:**
```
< | P-7.16 | Orphan children are reaped when the parent session terminates. | gateway-constitution-roadmap.md | `scheduler.rs::reap_orphaned_sessions`, `gateway_store/observability.rs::find_orphaned_sessions` | ENFORCED |
---
> | P-7.16 | Orphan children are reaped when the parent session terminates: their in-flight task records are Cancelled and their live execution handles aborted (scoped to the abandoned child, never its live siblings). Children parked at an approval gate are exempt. | gateway-constitution-roadmap.md; this amendment | `scheduler.rs::reap_orphaned_sessions` (handle abort via `active_executions().abort_workflow_tasks`), `tests/constitution_lifecycle_orphan_reaper_handle_abort.rs` | ENFORCED |
```
**Implementation:** PR #628. **Open question (NOT in this amendment):** the
reaper still deletes the abandoned child's checkpoint dir, conflicting with
"preserve forkability" — decide separately before encoding either way.

### Ri-0.12 — termination reasons (clarified) · READY (C2 + C1)

Confirm the budget circuit breaker (C2) terminates descendants under reason
**(b) budget exhaustion** (it is gateway-initiated, not operator — so not (c)),
and that C1 abandonment is reason **(d) parent-termination orphan reap**. Add one
sentence pinning the budget-breaker mapping; no structural change.

### NEW right — "the gateway enables choices; it never makes them" · READY (E1 / #622)

No existing clause states this (verified). New §0 Bill of Rights entry (Ri-x):

> Every gate output — every `GateKind`, to every decider (human or agent) —
> carries a typed `DecisionContext` sufficient to decide. The gateway supplies
> the data that makes a correct choice possible; it never substitutes its own
> judgment for the decider's (a place it would is a tracked DISCRETION LEAK).
> Mirror of O-1: O-1 binds the decider to give a reason; this binds the gateway
> to give the context. Human and agent deciders are governed by one rights/rules
> framework, differing only in authority/voting weight (ties to P-2.20/P-2.21;
> forward-compatible with a future voting decider model).

**Implementation:** PR #622 — `human_gate.rs::DecisionContext` (required on
`GateRequest`), `GateService::check` rejects insufficient context.

### P-5.14 — no change · (R1 / #627)

Already mandates "agent prose is the last-resort fallback only." R1 brings the
code into compliance (type-first classification, `classify_by_type`). Optionally
add `classify_by_type` to the enforcement column; no text change.

### PENDING #610 (divergence) — apply only after #610 merges

- **NEW sub-rule near P-7.19** — feedback-aware non-progress: repeated identical
  failure after feedback is a non-progress condition; distinct evolving errors
  (productive repair) are not. (D.1/D.6 — reconcile event IDs with #610.)
- **NEW — Sentinel is observational, never blocks** — the Sentinel classifies
  trajectory and notifies but never raises an execution-blocking gate; only the
  LoopGuard halts execution. (D.3 — pin with its test.)

## Related

- Umbrellas: #614 (determinism), #608 (divergence)
- Implementation: #622 (E1), #625 (C2), #627 (R1), #628 (C1), #629 (M1); pending #610 (D.1/D.3/D.6)
- Amendment package: `docs/constitution/pending-amendment-determinism-divergence.md`
- Follow-up: #630 (Gemma→driver + alias hard-reject)
