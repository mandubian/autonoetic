# RATIFY.md — Constitution Version 2026.07.30

## Summary

Constitutional amendment enacting the **data-egress label plane** as law
(#910), after the mechanics landed and were proven across phases 1–4 of the
data-envelopes RFC (`docs/rfc/data-envelopes-egress-localization.md`,
umbrella #903; phases #904–#909, follow-ups #961–#966).

Mechanics-first, per the amendment process: the enforcement shipped and was
tested before the clause was written. This version makes it *law*:

1. **New §15 (Data Egress Localization)** — three rules:
   - `P-15.1` — labeled content never reaches a sink its label excludes;
     withholding substitutes non-divulging indications; provider selection
     follows the taint.
   - `P-15.2` — every off-machine boundary (sandbox network, web, hooks,
     MCP, OFP, compression) gates on session taint before send.
   - `P-15.3` — widening happens only via operator-approved, scoped,
     expiring, revocable, causal-logged declassification grants.
2. **New §13 invariant `I-14`** — the egress label plane is gateway-only:
   agents never set, strip, or read labels; no hook or gateway-authored
   string may strip the label map or forge a declassification bypass.

Baseline: **2026.07.19**. Four clause-level changes — three new §15 rules,
one new §13 invariant bullet — plus the §0 prose ratio update
(`18 rights against 179 rules` → `18 rights against 182 rules`) to keep the
design-signal prose in sync with the mechanically counted lock numbers
(P-15.1 + P-15.2 + P-15.3 raise rules 179→182). No existing clause's text
or status changes; no entrenched clause is touched.

## Enforcement registration

- `enforcement_register.rs`: new parent principle `P-15` with entries for
  `P-15.1` / `P-15.2` / `P-15.3` (code + test citations; the register's
  citation-resolution test pins them).
- `fail_mode.rs`: new `RefuseTurn` fail mode — the honest declaration for
  mid-turn refusals (chokepoint assertion aborts, boundary gate refusals),
  distinct from `RefuseSessionStart`. Rows: `P-15.1`/`P-15.2`/`P-15.3`/`I-14`
  → `refuse-turn`.
- Egress causal events now carry their clause in `enforced_rules`
  (`P-15.1` for labeling/withholding/filtering/routing/assertion events,
  `P-15.2` for `egress.boundary_refused`, `P-15.3` for
  `egress.declassified`/`egress.relabel`) on top of the baseline
  attribution rule — contract-health attribution instead of `unattributed`.
- Constitutional tests: `tests/constitution/egress_data_localization.rs`
  (text-presence pins, register/fail-mode coverage, behavioral rule-ID
  attribution on emitted events).

## Federation ripple (P-10.9)

The constitution digest changes with this version. Peers in `Exact`
compatibility mode must add the new digest to `known_compatible_digests`
before federating with a gateway running this version.
