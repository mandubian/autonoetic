# RATIFY.md — Constitution Version 2026.07.19

## Summary

Constitutional amendment enacting, in a single signed batch, three staged
DRAFTs that had been accumulating under
`docs/constitution/amendments/`:

1. `2026-07-12-anomaly-reporting-DRAFT.md` — citizenship RFC Part C.1 (#770).
2. `2026-07-12-adjudication-sla-DRAFT.md` — citizenship RFC Part D.1 (#771).
3. `2026-07-14-genesis-one-door-DRAFT.md` — Agent Genesis — One Door RFC
   (a 2026-07-14 design audit finding that `skill_install` reached full
   activation while bypassing every §9 gate).

The citizenship pair makes anomaly reporting and its adjudication
*law* (the gateway code already ships — events carry `Ri-0.18`/`O-7` today
and contract-health was bucketing them `unattributed` until this version
lands). The genesis pair makes the single-door and import-provenance
guarantees *law* (the code also ships, via `skill_install` installing
Candidate-only + the `source_kind`/`source_ref` recording).

Baseline: **2026.07.08**. Six clause-level changes — one new right, one
replaced obligation + one new obligation, two new §9 sub-rules, one new
§13 invariant bullet — plus a §0 prose ratio update
(`17 rights against 177 rules` → `18 rights against 179 rules`) to keep
the design-signal prose in sync with the mechanically counted lock
numbers (Ri-0.18 raises rights 17→18; P-9.15 + P-9.16 raise rules
177→179). One existing clause's status changes: O-6 `PARTIAL` →
`ENFORCED` (the SLA mechanism ships in code, so it is no longer partial).
No entrenched clause's text or status changes; none of the new clauses
are entrenched (whether the correction machinery belongs in the
entrenched core is its own deliberate decision, not a rider here).

## Amendments

### §0 Bill of Rights — Ri-0.18 (new)

The right to file an anomaly report, **without holding any capability**,
durably recorded, non-repudiably attributed, never silently dropped, and
never itself grounds for sanction. The no-sanction sentence is what keeps
standing metrics honest — adjudication may score a flag's *precision* for
decision weight, but an honest-but-wrong flag is never punishable.

Triage bound (`max_pending_anomaly_flags_per_reporter`) is config, not
clause (O-1 lineage): a filing past the cap is *rejected loudly*
(`anomaly_flag_flood`), never silently dropped.

### §O Decider Obligations — O-6 (replaced, PARTIAL → ENFORCED) + O-7 (new)

**O-6** gains the bounded-window duty: a proposal left un-adjudicated past
the window is a recorded **breach** attributed to the adjudicating seat;
the breach does not resolve the proposal (the decision is still owed). The
status moves to `ENFORCED` because `scheduler.rs::check_adjudication_sla_breaches`
→ `constitutional_proposals.rs::flag_proposal_sla_breaches` now ships.

**O-7** is the sibling obligation for anomaly flags: a recorded decision
(`confirmed`/`dismissed`/`deferred`, with `under_review` as the non-terminal
holding state) within a bounded window. Both adjudication surfaces — the
operator's `anomaly.resolve` and the ombudsman office's `anomaly_adjudicate`
(RFC Part F, #774) — route through `anomaly_flags.rs::decide_anomaly_flag`,
and both owe the same motivation. Until the ombudsman office exists, the
anomaly review authority is the operator seat (the clause names the
*authority*, not the occupant, so moving the seat is config, not
re-amendment — the same office-before-occupant move as `GateDecider`
(P-2.20)).

O-7 is numbered past O-6 for the same reason O-6 was numbered past O-3/O-5:
those IDs are reserved for the principal-model RFC's planned obligations.

### §9 Agent Install & Provenance — P-9.15 (new) + P-9.16 (new)

**P-9.15 (Single door).** Every agent-activation surface passes the same
promotion gates (P-9.7 eval gating, P-9.9 high-risk capability review,
P-2.25 capability-delta approval). `skill_install` installs Candidate only;
activation flows through `agent_revision_promote`'s gate matrix. Sole
exception: gateway-startup bootstrap of the operator's own reference
bundles (`auto_promote: true` is parameter-explicit in `bootstrap.rs`).

**P-9.16 (Import provenance).** An agent installed from an external source
records `source_kind: "skill_install"` and `source_ref: "<url>#sha256=<digest>"`
on its revision and emits an `agent_install`/`skill_imported` causal event.
What arrived from outside, from where, and when is a permanent, queryable
fact — conceivable at install time and unrecorded can never be back-filled.

### §13 Cross-cutting invariants — I-13 (new, prose-only, no register entry)

**I-13 (Creation is not delegation.)** A newborn agent's capabilities come
through the gate, not from its creator's. Declared as a documented existing
deliberate absence: no attenuation check exists, by design. Consistent with
the rest of §13 (I-1 … I-12 are prose bullets; none carry register entries).

## Enforcement-register additions (committed alongside this version)

In `autonoetic-gateway/src/enforcement_register.rs`:

- 1 new `Principle { id: "P-9" }` (parent for P-9.15/P-9.16 — needed so
  `clause_exists("P-9")` resolves).
- 1 new `Right { id: "Ri-0.18" }`.
- 2 new `Obligation`s: `O-6` (was enacted law but missing from the
  register; now registered with SLA), `O-7`.
- 5 new `EnforcementEntry` rows, one per clause (Ri-0.18, O-6, O-7, P-9.15,
  P-9.16), matching the existing O-1/O-2 convention of one entry per
  obligation. No new entries for I-13 (see above).

`docs/constitution/enforcement-register.md` regenerated via
`BLESS_REGISTER=1 cargo test -p autonoetic-gateway bless_register_doc`.

## Lock counts

- `rule_enforcement_count`: 177 → **179** (P-9.15, P-9.16 added).
- `right_enforcement_count`: 17 → **18** (Ri-0.18 added).
- (O-* rows are not counted by either field; O-6/O-7 change the digest but
  not the counts.)
