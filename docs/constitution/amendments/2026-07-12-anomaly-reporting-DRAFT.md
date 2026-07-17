# DRAFT amendment — Anomaly reporting: Ri-0.18 (right to report) + O-7 (duty to adjudicate)

> **Status: DRAFT — NOT LAW.** This document is a ready-to-apply amendment
> package. The clauses below bind no one until they are added to a new
> constitution version and the lock is recomputed and **signed** (the signing
> key lives on another machine — see the enactment checklist). The
> implementing code (issue #770, citizenship RFC Part C.1,
> `docs/design/citizenship-as-a-runtime-service.md`) ships ahead of
> enactment: causal events already carry `Ri-0.18` / `O-7` so history is
> attributed from the moment the distinction is conceivable (philosophy
> §4.7, the append-only argument); contract-health buckets these as
> `unattributed` until the register learns the clauses at enactment.

## Rationale

Reporting unexpected behavior cannot rest on civic virtue — it must be
cheap, capability-free, and answered. The agent most likely to witness
misbehavior is the least privileged in the room, so the reporting surface
must require no capability (a **right**, binding the gateway). And a
reporting channel without a duty to decide teaches reporters that reporting
is pointless — extinguishing exactly the behavior it exists to elicit — so
every flag is owed a recorded, motivated decision (an **obligation**,
binding the decider). The pair mirrors Ri-0.8 + O-6 for amendment
proposals, with a different decision vocabulary and a different
adjudicating seat.

The two clauses are deliberately a *sibling pair* rather than an extension
of O-6: flags are a different object (confirmed/dismissed/deferred, not
approved/rejected), owed by a different authority (anomaly review seat, not
the proposal review authority), and clause narrowness is what keeps "a rule
without a test is a wish" workable.

## Clause text

### Insert into §0 Bill of Rights, after the Ri-0.17 row:

```markdown
| Ri-0.18 | Any agent may file an anomaly report (`anomaly_flag`) at any time, **without holding any capability**. The gateway records every flag durably, attributed non-repudiably to the reporting principal (Ri-0.11), and a flag cannot be silently dropped. Filing a flag is never, by itself, grounds for sanction against the reporter. | The agent most likely to witness misbehavior may be the least privileged in the room; a reporting surface gated by capability filters out precisely the witnesses that matter. The no-sanction sentence keeps standing metrics honest: adjudication may score a flag's *precision* for decision weight, but an honest-but-wrong flag is never punishable — otherwise reporting collapses into risk-avoidance. | `runtime/tools/anomaly_flag.rs::AnomalyFlagTool` (Core tier via `config/tools.yaml`, `is_available` unconditionally true); durable row in `scheduler/gateway_store/anomaly_flags.rs`; causal event `anomaly_flag.filed` with `enforced_rules: ["Ri-0.18"]`; pinned by `tests/anomaly_flag_integration.rs` (zero-capability manifest files a flag) and `tool_tier_registry` Core-tier pin. | ENFORCED |
```

### Insert into §O Decider Obligations, after the O-6 row:

```markdown
| O-7 | An anomaly review authority owes every Ri-0.18 flag a **recorded decision** (`confirmed`/`dismissed`/`deferred`, with `under_review` as the non-terminal holding state) with motivation once actioned, **within a bounded adjudication window**. Intake alone is not adjudication, and neither is indefinite deferral — a reporting channel without a duty to decide *on time* teaches reporters that reporting is pointless, and extinguishes the behavior it exists to elicit. A flag left un-adjudicated past the window is a recorded **breach** attributed to the adjudicating seat; the breach does not resolve the flag (the decision is still owed). Window duration is config (O-1 lineage). | Ri-0.18, O-1 | `anomaly.resolve` records the motivated decision (terminal decisions require a non-empty reason, gated by `decider_obligations.enabled`; refusal/satisfaction emitted as `decider_obligation` causal events with `enforced_rules: ["O-7"]`); `scheduler.rs::check_adjudication_sla_breaches` → `anomaly_flags.rs::flag_anomaly_flag_sla_breaches` stamps `sla_breached_at` once and emits a `decider_obligation`/`sla_breached` event (`["O-7"]`) plus a reporter notification, gated by `adjudication_sla_secs`; visibility via `anomaly.list_pending`. | PARTIAL |
```

Numbering note: O-7 is numbered past O-6 for the same reason O-6 was
numbered past O-3/O-5 — those IDs are reserved for the principal-model
RFC's planned obligations (anti-fatigue, scope honesty,
duty-to-escalate-not-reject).

Seat note: until the ombudsman office (RFC Part F, issue #773) exists, the
anomaly review authority is the operator seat. The clause names the
*authority* (seat, not occupant) so moving it to an agent-decider later is
configuration, not re-amendment — the same office-before-occupant move as
`GateDecider` (P-2.20).

Triage-bound note: Ri-0.18's "cannot be silently dropped" is compatible with
a bounded intake queue, and the bound is **config, not clause** (O-1 lineage
— same answer as O-7's SLA window and P-7.17's approval flood cap;
citizenship RFC open question 2). `max_pending_anomaly_flags_per_reporter`
(default 50, 0 disables) caps un-adjudicated flags per reporter: a filing
beyond the cap is *rejected loudly* (`anomaly_flag_flood` error to the
reporter plus a once-per-window operator notification), which is the
opposite of a silent drop — the reporter is told, and terminal
adjudications free capacity, so the bound pressures the *decider* to keep
up, never deletes a report. Constitutional text stays silent on the number
for the same reason it stays silent on the SLA seconds.

## Enforcement-register additions (at enactment, same commit as the new version)

In `autonoetic-gateway/src/enforcement_register.rs`:

```rust
// In rights():
Right {
    id: "Ri-0.18",
    title: "Right to report",
    statement: "Any agent may file an anomaly report without holding any capability; every flag is durably recorded, non-repudiably attributed, cannot be silently dropped, and filing is never itself grounds for sanction.",
    entrenched: false,
},

// In obligations():
Obligation {
    id: "O-7",
    title: "Duty to adjudicate reports",
    statement: "An anomaly review authority owes every Ri-0.18 flag a recorded decision (confirmed/dismissed/deferred) with motivation once actioned, per O-1. Intake alone is not adjudication.",
    entrenched: false,
},

// In enforcement_register():
EnforcementEntry {
    clause_id: "Ri-0.18",
    rule_id: "Ri-0.18",
    check_id: "anomaly_flag_capability_free_intake",
    code: "runtime/tools/anomaly_flag.rs::AnomalyFlagTool (is_available == true, Core tier) + gateway_store/anomaly_flags.rs durable row + causal event anomaly_flag.filed",
    test: "anomaly_flag_integration.rs::zero_capability_manifest_can_file_flag",
    config: None,
},
EnforcementEntry {
    clause_id: "Ri-0.18",
    rule_id: "Ri-0.18",
    check_id: "anomaly_flag_intake_triage_bound",
    code: "gateway_store/anomaly_flags.rs::insert_anomaly_flag per-reporter flood cap (loud anomaly_flag_flood rejection + emit_anomaly_flag_flood_alert operator notification, once per flood window)",
    test: "anomaly_flags.rs::tests::flood_cap_rejects_at_limit_and_keeps_existing + anomaly_flag_integration.rs::flood_cap_rejects_filing_loudly",
    config: Some("max_pending_anomaly_flags_per_reporter"),
},
EnforcementEntry {
    clause_id: "O-7",
    rule_id: "O-7",
    check_id: "anomaly_flag_adjudication_motivation",
    code: "router.rs::anomaly.resolve -> decide_anomaly_flag; terminal decision requires non-empty motivation (decider_obligations.enabled)",
    test: "router.rs::tests::test_dispatch_anomaly_resolve_requires_motivation",
    config: Some("decider_obligations.enabled"),
},
EnforcementEntry {
    clause_id: "O-7",
    rule_id: "O-7",
    check_id: "anomaly_flag_adjudication_sla",
    code: "scheduler.rs::check_adjudication_sla_breaches -> anomaly_flags.rs::flag_anomaly_flag_sla_breaches (sla_breached_at, decider_obligation/sla_breached event)",
    test: "scheduler.rs::adjudication_sla_tests::breaches_are_recorded_without_changing_status",
    config: Some("decider_obligations.adjudication_sla_secs"),
},
```

(Adjust `test:` names to the actual test identifiers if they differ after
review; the register's `required_fields_are_non_empty` and
`clause_check_pairs_are_unique` tests will police the shape.)

**Do not mark either clause `entrenched`** — the entrenched correction core
is a hard-coded list guarded by
`entrenched_clauses_are_the_expected_correction_core`; whether the
reporting channel belongs in the core is a separate, deliberate decision
(arguably yes, eventually — it is correction machinery — but that is its
own amendment discussion, not a rider on this one).

## Enactment checklist (on the machine holding the signing key)

1. Create `docs/constitution/versions/<new-date>/` as a copy of
   `2026.07.08/`; apply the two row insertions above to its
   `constitution.md`.
2. Recompute and sign the lock:
   `python3 docs/constitution/recompute_lock.py --version <new-date> --signing-sk-b64 "$AUTONOETIC_CONSTITUTION_SIGNING_SK_B64"`
   (the script recounts enforcement rows — Ri count +1 — and embeds the new
   digest).
3. Bump `ACTIVE_CONSTITUTION_VERSION` in `autonoetic-types/src/config.rs`
   (currently line ~819, `"2026.07.08"`) to the new date; grep for other
   hardcoded `2026.07.08` references (README.md, docs/README.md,
   CLAUDE.md's recompute example) and update.
4. Apply the enforcement-register additions above; regenerate the committed
   register doc: `BLESS_REGISTER=1 cargo test -p autonoetic-gateway bless_register_doc`
   and commit the regenerated `docs/constitution/enforcement-register.md`.
5. Add text-presence pins for the new clauses (mirror
   `tests/constitution_2026_07_08_new_clauses_present.rs`).
6. Validate:
   `cargo test -p autonoetic-gateway constitution_lock_matches_canonical_digest_and_counts`
   plus the enforcement_register test module.
7. Delete this draft file (its content is now law + register).

## Interaction with the current unsigned-lock state

The repo's `2026.07.08` lock is currently out of sync with its markdown
(pre-existing digest mismatch, awaiting the same signing machine). Step 2
supersedes that repair if the new version is enacted directly; otherwise
re-sign `2026.07.08` first, then amend.
