# DRAFT amendment — Adjudication timeliness: O-6 SLA (and O-7 SLA)

> **Status: DRAFT — NOT LAW.** Ready-to-apply amendment package; binds no one
> until a new constitution version carries it and the lock is recomputed and
> **signed** (signing key is on another machine — see the enactment
> checklist). Implements citizenship RFC Part D.1 (issue #771),
> `docs/design/citizenship-as-a-runtime-service.md`.
>
> **Sign together with** the companion draft
> [`2026-07-12-anomaly-reporting-DRAFT.md`](2026-07-12-anomaly-reporting-DRAFT.md)
> (Ri-0.18 + O-7): this amendment sharpens O-7 with the same timeliness
> mechanism, so O-7's final enacted form should already include the SLA (that
> draft's O-7 row has been updated accordingly). O-6 below modifies an
> **already-enacted** clause.
>
> The SLA *mechanism* already ships in code (PR for #771 D.1): a scheduler
> scan stamps `sla_breached_at` once on any proposal/flag left un-adjudicated
> past `decider_obligations.adjudication_sla_secs`, emits a
> `decider_obligation` / `sla_breached` causal event tagged `O-6`/`O-7`, and
> notifies the proposer/reporter. Until this amendment + the O-6/O-7 register
> entries are enacted, those breach events bucket as `unattributed` in
> contract-health — history is attributed from the moment conceivable
> (philosophy §4.7), attribution goes live at signing.

## Rationale

O-6 today owes a *recorded decision* but sets no deadline, so the duty is
satisfiable by never deciding — a petition system with intake but no
timeliness degrades into a suggestion box that files petitions politely and
ignores them (`docs/philosophy.md` §5, gap 2). For non-deterministic agents
this is not merely unfair: a proposal that sits pending indefinitely
*extinguishes proposing behavior in the models themselves* — voice that is
never answered is unlearned. The same holds for anomaly flags (O-7): a
reporting channel whose reports vanish teaches reporters that reporting is
pointless.

Consistent with O-1's design, **the duty is constitutional; the threshold is
config**. This amendment establishes that timeliness is *owed* (a bounded
adjudication window); the specific window (`adjudication_sla_secs`, default 7
days, `0` = disabled) is an operational knob, exactly as O-1 says
"Tiers/thresholds are config, not constitution." A breach does **not** resolve
or cancel the item — the decision is still owed; it records that the deadline
passed, once, and surfaces it against the adjudicating seat in contract-health.

## Clause text

### Replace the enacted O-6 row (§O) with:

```markdown
| O-6 | A proposal review authority owes every Ri-0.8 proposal a **recorded decision** (`approved`/`rejected`/`deferred`/`under_review`) with motivation once actioned, **within a bounded adjudication window**. Intake alone (Ri-0.8: "cannot be silently dropped") is not adjudication, and neither is indefinite deferral — a petition system without a duty to decide *on time* degrades into a suggestion box. A proposal left un-adjudicated past the window is a recorded **breach** attributed to the adjudicating seat; the breach does not resolve the proposal (the decision is still owed). The window duration is config, not constitution (O-1 lineage). | Ri-0.8, O-1 | `constitution.resolve_proposal` records the decision; `scheduler.rs::check_adjudication_sla_breaches` → `constitutional_proposals.rs::flag_proposal_sla_breaches` stamps `sla_breached_at` once and emits a `decider_obligation`/`sla_breached` causal event (`enforced_rules: ["O-6"]`) plus a proposer notification, gated by `decider_obligations.enabled` + `adjudication_sla_secs`; visibility via `constitution.list_pending_proposals`. | ENFORCED |
```

(Changes from the enacted text: adds the bounded-window duty, the breach
semantics, and the config-threshold note; upgrades Status `PARTIAL` →
`ENFORCED`.)

### In the companion anomaly-reporting draft, the O-7 row is updated to include the SLA:

O-7's `docs/constitution/amendments/2026-07-12-anomaly-reporting-DRAFT.md`
row gains the same "within a bounded adjudication window" duty + breach
semantics, and its enforcement cell cites
`anomaly_flags.rs::flag_anomaly_flag_sla_breaches` alongside the existing
`anomaly.resolve` recording. (Applied in that file so O-7 enacts complete.)

## Enforcement-register additions (at enactment)

In `autonoetic-gateway/src/enforcement_register.rs`:

**O-6 is enacted law today but missing from the code register** (only O-1/O-2
are registered) — this closes that gap *and* wires the SLA:

```rust
// In obligations():
Obligation {
    id: "O-6",
    title: "Duty to adjudicate proposals, on time",
    statement: "A proposal review authority owes every Ri-0.8 proposal a recorded, motivated decision within a bounded adjudication window; a proposal left un-adjudicated past the window is a recorded breach attributed to the adjudicating seat (the decision is still owed). Window duration is config.",
    entrenched: false,
},

// In enforcement_register():
EnforcementEntry {
    clause_id: "O-6",
    rule_id: "O-6",
    check_id: "proposal_adjudication_recorded",
    code: "router.rs::constitution.resolve_proposal -> decide_constitutional_proposal",
    test: "router.rs::tests::test_dispatch_constitution_resolve_proposal",
    config: None,
},
EnforcementEntry {
    clause_id: "O-6",
    rule_id: "O-6",
    check_id: "proposal_adjudication_sla",
    code: "scheduler.rs::check_adjudication_sla_breaches -> constitutional_proposals.rs::flag_proposal_sla_breaches (sla_breached_at, decider_obligation/sla_breached event)",
    test: "scheduler.rs::adjudication_sla_tests::breaches_are_recorded_without_changing_status",
    config: Some("decider_obligations.adjudication_sla_secs"),
},
```

The O-7 SLA register entry (a second `EnforcementEntry` for `O-7` citing
`flag_anomaly_flag_sla_breaches`) is added to the O-7 block in the companion
anomaly draft.

**Do not mark O-6/O-7 `entrenched`.** (Same reasoning as the anomaly draft:
whether the correction machinery belongs in the entrenched core is its own
deliberate decision, not a rider here.)

## Enactment checklist (on the signing machine)

1. In the new constitution version dir, apply **both** this amendment (O-6
   replacement) and the companion anomaly draft (Ri-0.18 + O-7, O-7 already
   carrying its SLA) to `constitution.md`.
2. Recompute + sign the lock (`recompute_lock.py --version <new-date>` with
   the signing key). The enforcement-row count rises (Ri +1, O +2: O-6 and
   O-7 both newly enforcement-registered in markdown).
3. Bump `ACTIVE_CONSTITUTION_VERSION` in `autonoetic-types/src/config.rs`.
4. Apply the register additions above (O-6) and in the anomaly draft (O-7);
   regenerate `docs/constitution/enforcement-register.md` via
   `BLESS_REGISTER=1 cargo test -p autonoetic-gateway bless_register_doc`.
5. Reconcile the register `test:` reference above with the actual SLA test
   name from the shipped code.
6. Add text-presence pins for the new clauses (mirror
   `tests/constitution_2026_07_08_new_clauses_present.rs`).
7. Validate: `constitution_lock_matches_canonical_digest_and_counts` + the
   `enforcement_register` test module. Delete both draft files.
