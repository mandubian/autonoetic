# Pending Constitution Amendment — Determinism + Divergence (#621)

**Status:** PREPARED, not yet applied/signed. This is the one-pass execution
package for #621. **Do not apply piecemeal.** Apply all READY clauses + the
PENDING clauses (once their code merges), then run `recompute_lock.py` **once**
with the signing key, then validate.

**Baseline:** `docs/constitution/versions/2026.06.22/constitution.md`.
**New version dir to create at apply time:** pick the apply date, e.g.
`docs/constitution/versions/2026.06.24/`.

## Hard prerequisites before signing
1. **Determinism (#614) — DONE:** E1 (#622), C2 (#625), R1 (#627), C1 (#628),
   M1 (#629) merged. The clauses below marked READY are safe to apply now.
2. **Divergence (#608) — PENDING:** clauses marked PENDING depend on **#610**
   (D.1/D.6 feedback, D.3 Sentinel-never-blocks). Do not apply them until #610
   merges and its causal-event IDs are final.
3. **Signing key:** `AUTONOETIC_CONSTITUTION_SIGNING_SK_B64` (operator-held). The
   lock signature cannot be recomputed without it.

## Clause changes

### READY — code merged

**1. P-6.21 (tree budget) — amend.** *(C2 / #625)*
- Current: "Tree-wide budget aggregated across all descendants of a root session."
- New: append — "On exhaustion the gateway **cancels in-flight descendants**
  (graceful budget circuit breaker, reusing the emergency-stop cascade) so they
  cannot keep spending an already-exhausted tree budget; the breaker fires once
  per root (idempotent)."
- Enforcement col: add `execution.rs::trigger_root_budget_circuit_breaker`,
  `tests/root_budget_circuit_breaker.rs`.

**2. P-7.5 (tool-failure budget) — amend.** *(D.2 / divergence #620)*
- Current: "…; permission errors do not count."
- New: "…; **irrecoverable errors do not count** — permission, quota_exceeded,
  sandbox_unavailable, and signal-derived exits (exit_code ≥ 128, `ok:false`).
  These are surfaced separately as a blocked-state, not divergence."
- Enforcement col: add `guard.rs::is_irrecoverable`.

**3. P-7.16 (orphan reap) — amend.** *(C1 / #628)*
- Current: "Orphan children are reaped when the parent session terminates."
- New: "…reaped when the parent terminates: their in-flight task records are
  Cancelled **and their live execution handles aborted** (scoped to the
  abandoned child, never its live siblings). Children parked at an approval gate
  are exempt." 
- Enforcement col: add the `abort_workflow_tasks` call in
  `scheduler.rs::reap_orphaned_sessions`,
  `tests/constitution_lifecycle_orphan_reaper_handle_abort.rs`.
- NOTE (open question, not part of this amendment): the reaper still **deletes**
  the abandoned child's checkpoint dir, which conflicts with "preserve
  forkability." Decide separately (see #630 / RFC C1 note); do not encode either
  way here until decided.

**4. Ri-0.12 (closed termination-reason list) — clarify.** *(C2 + C1)*
- The budget circuit breaker (C2) terminates descendants via the emergency-stop
  mechanism — confirm this maps to reason **(b) budget exhaustion** (preferred)
  or **(c) operator emergency stop**, and add one clause sentence pinning that a
  gateway-initiated budget breaker is reason (b), not (c) (it is not operator
  initiated). C1 abandonment already maps to **(d) parent-termination orphan
  reap** — no change needed there.

**5. NEW right — "The gateway enables choices; it never makes them."** *(E1 / #622)*
- No existing clause states this (verified). Add to §0 Bill of Rights (Ri-x) or
  §O. Proposed text: "Every gate output — every `GateKind`, to every decider
  (human or agent) — carries a typed `DecisionContext` sufficient to decide. The
  gateway supplies the data that makes a correct choice possible; it never
  substitutes its own judgment for the decider's (a place it would is a tracked
  DISCRETION LEAK). This is the gateway's mirror of O-1: O-1 binds the decider to
  give a reason; this binds the gateway to give the context."
- **Decider symmetry:** add that human and agent deciders are governed by one
  rights/rules framework, differing only in authority/voting weight (ties to
  P-2.20/P-2.21; forward-compatible with a future voting decider model).
- Enforcement col: `human_gate.rs::DecisionContext` (required on `GateRequest`),
  `GateService::check` rejects insufficient context.

### OPTIONAL — code merged

**6. O-1 (decider motivation) — optionally extend.** *(E1)*
- State the escalation-context tiers (Tier 1/2/3) explicitly, or leave as a
  code-level tightening (the typed `DecisionContext` already enforces it). Defer
  unless you want the tiers in signed text.

### NO CHANGE — already satisfied

**7. P-5.14 (mechanical failure classification).** *(R1 / #627)*
- Already mandates "agent prose is the last-resort fallback only." R1 brings the
  code into compliance (type-first classification). **No text change**; optionally
  add `classify_by_type` to the enforcement col.

### PENDING — do not apply until #610 (divergence) merges

**8. NEW sub-rule near P-7.19 — feedback-aware non-progress.** *(D.1/D.6)*
- Draft: "The loop guard / Sentinel also recognises **repeated identical failure
  after feedback was given** as a non-progress condition; distinct evolving
  errors (productive repair) do not count." Reconcile exact wording + the
  causal-event IDs with #610 when it lands.

**9. NEW — "the Sentinel is observational; it never blocks execution."** *(D.3)*
- Draft (Ri-x or §O invariant): "The Sentinel observes and classifies trajectory
  (Blocked / Diverging / Watching) and notifies, but never raises an
  execution-blocking gate. Only the LoopGuard halts execution." Pin with the D.3
  test. Reconcile with #610.

## enforcement_register entries to add (code)
Add/extend entries mapping these new causal events / checks to their clause:
`operator_alert.blocked_state` → P-7.5 (D.2); the budget breaker stop →
P-6.21 (C2); the reaper handle-abort → P-7.16 (C1); `DecisionContext`
enforcement → the new E1 right; (pending) feedback events + Sentinel verdicts →
the new P-7.19 sub-rule / Sentinel right. Verify `contract_health` shows no
`unattributed` for the new events.

## Operator execution (one pass, after #610 merges)
```bash
# 1. Create the new version dir and apply ALL clause edits above (READY + PENDING).
cp -r docs/constitution/versions/2026.06.22 docs/constitution/versions/<APPLY_DATE>
$EDITOR docs/constitution/versions/<APPLY_DATE>/constitution.md   # apply clauses 1-9
# Move the prepared RATIFY draft into the new version dir (and fill in the version/date):
git mv docs/constitution/RATIFY-pending-determinism-divergence.md \
       docs/constitution/versions/<APPLY_DATE>/RATIFY.md

# 2. Recompute the lock (digest + signature) — REQUIRES the signing key:
python3 docs/constitution/recompute_lock.py --version <APPLY_DATE> \
  --signing-sk-b64 "$AUTONOETIC_CONSTITUTION_SIGNING_SK_B64"

# 3. Add the enforcement_register entries (code) for the new events.

# 4. Validate:
cargo test -p autonoetic-gateway constitution_lock_matches_canonical_digest_and_counts
cargo test -p autonoetic-gateway --test constitution_r_8_6_retention_policy_startup
cargo test -p autonoetic-gateway   # full suite
```
