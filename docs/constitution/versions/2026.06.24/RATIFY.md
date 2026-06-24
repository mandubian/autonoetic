# RATIFY.md — Constitution Version 2026.06.24

## Summary

Constitutional amendments for the gateway **determinism** (#614) and
**divergence-robustness** (#608) work. Makes already-enforced gateway behaviour
lawful in signed text: irrecoverable-error handling, root-budget cascade,
graceful child abandonment, a new decider-context right, and the
Sentinel-observational invariant. Baseline: **2026.06.22**. Three amended
clauses, one clarified, two new rights.

**Status at ratification:** every clause's enforcing code is merged
(determinism E1 #622 / C2 #625 / R1 #627 / C1 #628 / M1 #629; divergence
D.1/D.6/D.3 via #623). The lock (digest + signature) must be recomputed with the
operator signing key — see "Recompute" below.

## Amendments

### P-6.21 — tree-budget cascade on exhaustion (amended) · C2 / #625

```
< | P-6.21 | Tree-wide budget aggregated across all descendants of a root session. | gateway-constitution-roadmap.md | `runtime/root_session_budget.rs` `runtime/lifecycle.rs:1254` | ENFORCED |
---
> | P-6.21 | Tree-wide budget aggregated across all descendants of a root session. On exhaustion the gateway cancels in-flight descendants (a graceful budget circuit breaker reusing the emergency-stop cascade), fired exactly once per root (idempotent), so descendants cannot keep spending an already-exhausted tree budget. | gateway-constitution-roadmap.md; this amendment | `runtime/root_session_budget.rs`, `execution.rs::trigger_root_budget_circuit_breaker`, `root_budget_circuit_breaker.rs` | ENFORCED |
```

### P-7.5 — irrecoverable errors do not count (amended) · D.2 / #620

```
< | P-7.5 | Loop guard trips on `max_tool_failures` per tool (...); permission errors do not count. | ARCHITECTURE.md | `guard.rs::register_failure` + `check_loop` | ENFORCED |
---
> | P-7.5 | Loop guard trips on `max_tool_failures` per tool (...); irrecoverable errors do not count — permission, quota_exceeded, sandbox_unavailable, and signal-derived exits (exit_code ≥ 128, `ok:false`). These are surfaced as a blocked-state (`operator_alert.blocked_state`), not divergence. | ARCHITECTURE.md; this amendment | `guard.rs::is_irrecoverable` + `register_failure` + `check_loop` | ENFORCED |
```

### P-7.16 — graceful child abandonment aborts live handles (amended) · C1 / #628

```
< | P-7.16 | Orphan children are reaped when the parent session terminates. | gateway-constitution-roadmap.md | `scheduler.rs::reap_orphaned_sessions`, ... | ENFORCED |
---
> | P-7.16 | Orphan children are reaped when the parent session terminates: their in-flight task records are Cancelled and their live execution handles aborted (scoped to the abandoned child, never its live siblings). Children parked at an approval gate are exempt. | gateway-constitution-roadmap.md; this amendment | `scheduler.rs::reap_orphaned_sessions` (handle abort via `active_executions().abort_workflow_tasks`), ..., `constitution_lifecycle_orphan_reaper_handle_abort.rs` | ENFORCED |
```
**Open question (NOT encoded):** the reaper still deletes the abandoned child's
checkpoint dir, conflicting with "preserve forkability." Decide separately
(#630 / RFC C1 note) before encoding either way.

### Ri-0.12 — termination reasons (clarified) · C2 + C1

Added one sentence: a gateway-initiated budget circuit breaker that cancels
in-flight descendants on tree-budget exhaustion (P-6.21) is reason **(b) budget
exhaustion**, not operator emergency stop (c). C1 abandonment remains reason
**(d) parent-termination orphan reap** (unchanged).

### Ri-0.15 — "the gateway enables choices; it never makes them" (new) · E1 / #622

New §0 Bill of Rights entry. Every gate output — every `GateKind`, to every
decider (human or agent) — carries a typed `DecisionContext` sufficient to
decide; the gateway supplies the data and never substitutes its own judgment
(a place it would is a DISCRETION LEAK). Mirror of O-1 (decider owes a reason ↔
gateway owes the context). Decider symmetry: human and agent deciders share one
rights/rules framework, differing only in authority/voting weight (ties to
P-2.20/P-2.21). **Enforcement:** required `DecisionContext` on
`human_gate.rs::GateRequest`; `GateService::check` rejects empty/boilerplate
context; `constitution_gate_enforced_rules.rs`.

### Ri-0.16 — the Sentinel is observational; it never blocks (new) · D.1/D.6/D.3 / #623

New §0 Bill of Rights entry. The divergence Sentinel classifies trajectory and
emits `divergence.*` events + a non-blocking operator alert but never raises an
execution-blocking gate — only the LoopGuard halts. Only a confirmed
`FeedbackIgnored` signal may drive `Diverging`/`Critical`; every other signal is
capped at `Watching` (advisory). **Enforcement:** `trajectory_health.rs`
(advisory-only `aggregate`, `FeedbackIgnored`, `is_advisory_only`); feedback
events in `execution.rs`; `trajectory_monitor_integration.rs`.

### P-5.14 — no change · R1 / #627

Already mandates "agent prose is the last-resort fallback only." R1 brings the
code into compliance (type-first `classify_by_type`); no clause-text change.

## Activation (wired in this PR)

- `autonoetic-types/src/config.rs`: `default_constitution_source_path` /
  `default_constitution_lock_path` → `versions/2026.06.24/`.
- `docs/constitution/CURRENT` → `2026.06.24`.

## Recompute (operator — REQUIRED before the constitution tests pass)

The lock digest + signature are NOT in this PR (signing needs the operator key).
Until recomputed, the constitution-init tests are red **by design**.

```bash
python3 docs/constitution/recompute_lock.py --version 2026.06.24 \
  --signing-sk-b64 "$AUTONOETIC_CONSTITUTION_SIGNING_SK_B64"
cargo test -p autonoetic-gateway constitution_lock_matches_canonical_digest_and_counts
cargo test -p autonoetic-gateway --test constitution_r_8_6_retention_policy_startup
cargo test -p autonoetic-gateway
git add docs/constitution/versions/2026.06.24/gateway-constitution.lock.json && git commit
```

Also add `enforcement_register` entries (code) for the new causal events so
`contract_health` shows no `unattributed`: `operator_alert.blocked_state` →
P-7.5; budget breaker stop → P-6.21; reaper handle-abort → P-7.16;
`DecisionContext` enforcement → Ri-0.15; `FeedbackIgnored` / Sentinel verdicts →
Ri-0.16.

## Related

- Umbrellas: #614 (determinism), #608 (divergence) · Tracker: #621
- Implementation: #622 (E1), #625 (C2), #627 (R1), #628 (C1), #629 (M1), #623 (D.1/D.3/D.6)
- Follow-up: #630 (Gemma→driver + alias hard-reject); D.7 popup→advisory (still open under #610)
