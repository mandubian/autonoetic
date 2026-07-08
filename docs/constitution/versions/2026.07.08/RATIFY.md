# RATIFY.md — Constitution Version 2026.07.08

## Summary

Constitutional amendment opening five doors named in `docs/philosophy.md`
(the maintainer's stated commitments: correctability over perfection, a
democratic trajectory for agents, end-user primacy, iterative
constitutionalism) so later evolution does not require re-architecture:

1. A new **§12 Rights of the Served** — `U-1`/`U-2`/`U-3` — giving the
   party a session serves (distinct from the operator) explicit, if not yet
   enforced, standing before any decider power spreads to agents.
2. **`Ri-0.17`** — the right to request export of one's own cognitive
   capsule (emigration), declared `PARTIAL` against the capsule-export tool
   that already permits this in practice (but also permits more).
3. **`O-6`** — a proposal-review authority's duty to record a decision on
   every `Ri-0.8` amendment proposal, closing the gap between "cannot be
   silently dropped" (intake) and actual adjudication. Numbered past the
   RFC #359-reserved `O-3`/`O-4`/`O-5` block to avoid colliding with those
   planned obligations.
4. **`I-12`** — a Sybil-collapse invariant for any future collective
   decision mechanism, declared before any such mechanism exists.
5. An **entrenched clauses** paragraph in the Amendment Process, naming
   `Ri-0.2`, `Ri-0.3`, `Ri-0.8`, `Ri-0.11`, `O-1` as the correction core and
   requiring extra, dated justification to weaken or remove any of them.

Baseline: **2026.07.02**. Six additions (one new section, three rights-table
rows across §0/§12, one obligation, one invariant), plus one process
paragraph. No existing clause's text or status changed.

## Amendments

### New §12 — Rights of the Served

```
| U-1 | The served party may refuse a delivered result, without penalty and without needing to justify the refusal. | ... | None yet. | MISSING |
| U-2 | The served party may obtain a plain-language account of what was done on their behalf ... | ... | Partial raw material exists ... | MISSING |
| U-3 | On exit, the served party may obtain or require deletion of the data held on their behalf. | ... | None yet. | MISSING |
```

Full text in `constitution.md` §12, including the bind-direction paragraph
(binds the community — gateway + agents collectively — toward the served
party) and its reference to the new `PrincipalKind::ServedUser` attribution
infrastructure (`autonoetic-types/src/principal.rs`).

### Ri-0.17 — Right to request capsule export · emigration

```
| Ri-0.17 | An agent may request export of its own cognitive capsule for migration to another gateway. | ... | `runtime/tools/capsule.rs::CapsuleExportTool` gated by `Capability::CapsuleExport` — currently broader than self-export ... A scoped `SelfCapsuleExport` capability ... is named but not yet enacted ... | PARTIAL |
```

### O-6 — Proposal adjudication duty

```
| O-6 | A proposal review authority owes every Ri-0.8 proposal a recorded decision (approved/rejected/deferred/under_review) with motivation once actioned. ... | Ri-0.8, O-1 | constitution.resolve_proposal JSON-RPC method ... calling decide_constitutional_proposal; visibility via constitution.list_pending_proposals. Covers the recording half; no timeliness/SLA enforcement yet ... | PARTIAL |
```

### I-12 — Sybil-collapse precondition for collective decisions

```
- **I-12** Any collective decision mechanism among principals (voting,
  weighted advisory verdicts, or any future franchise) must collapse an
  agent and its spawn-descendants into a single principal for weight
  purposes — extending P-10.7's spawn-tree trust-boundary collapse ...
  (DESIGN DEBT — no collective decision mechanism exists to enforce this
  against yet; tracked as a precondition for any future one.)
```

### Amendment Process — Entrenched clauses paragraph

New paragraph naming `Ri-0.2`, `Ri-0.3`, `Ri-0.8`, `Ri-0.11`, `O-1` as the
correction core (`docs/philosophy.md` §3.1/§4.1) and requiring an explicit,
dated `RATIFY.md` justification, on top of the existing sign-off
requirements, for any amendment that weakens or removes one of them.

## Activation (wired in this change)

- `autonoetic-types/src/principal.rs`: `PrincipalKind::ServedUser` variant +
  `"user:<id>"` parsing in `decider_principal_kind()` — infrastructure for
  §12's bind-direction, not yet emitted by any call site.
- `autonoetic-gateway/src/enforcement_register.rs`: `rights()` expanded from
  2 to 6 entries (`Ri-0.2`, `Ri-0.3`, `Ri-0.8`, `Ri-0.11` added alongside the
  existing `Ri-0.13`/`Ri-0.14`), each with a matching `EnforcementEntry`
  citing real, existing code and tests; `entrenched: bool` added to `Right`
  and `Obligation`; `entrenched_clauses()` + `dead_clauses()` added;
  `docs/constitution/enforcement-register.md` regenerated
  (`BLESS_REGISTER=1`).
- `autonoetic-gateway/src/router.rs`: `constitution.resolve_proposal` and
  `constitution.list_pending_proposals` JSON-RPC methods (O-6).
- `autonoetic/src/cli/trace.rs`: `trace contract-health` surfaces a
  "never enforced in window" section from `dead_clauses()`.
- `autonoetic/src/cli/room/render.rs`: `actor_label` handles
  `PrincipalKind::ServedUser`.
- Pinned tests: `autonoetic-types/src/principal.rs` (unit),
  `autonoetic-gateway/src/enforcement_register.rs` (unit — dead-clause and
  entrenchment structural backstop), `autonoetic-gateway/src/router.rs`
  (`test_dispatch_constitution_resolve_proposal*` — integration-style,
  in-module per the existing `test_dispatch_constitution_get` pattern),
  `autonoetic-gateway/tests/constitution_2026_07_08_new_clauses_present.rs`
  (text-presence pin for this amendment's drafted clauses).

**Activated.** The lock was recomputed with the operator signing key;
`docs/constitution/CURRENT` points to `2026.07.08` and
`autonoetic-types/src/config.rs::ACTIVE_CONSTITUTION_VERSION` is set to
`"2026.07.08"`. `gateway-constitution.lock.json` for this version is signed
under `autonoetic:constitution:v1`.

## Recompute (record of how this version was activated)

This version was activated by running `recompute_lock.py` with the operator
signing key, then bumping the pointers:

```bash
python3 docs/constitution/recompute_lock.py --version 2026.07.08 \
  --signing-sk-b64 "$AUTONOETIC_CONSTITUTION_SIGNING_SK_B64"
# Pointers bumped:
#   docs/constitution/CURRENT -> 2026.07.08
#   autonoetic-types/src/config.rs::ACTIVE_CONSTITUTION_VERSION -> "2026.07.08"
#     (+ default_constitution_source_path / default_constitution_lock_path)
# Regenerate the glossary against the now-active version:
BLESS_GLOSSARY=1 cargo test -p autonoetic-gateway bless_constitution_glossary
cargo test -p autonoetic-gateway constitution_lock_matches_canonical_digest_and_counts
git add docs/constitution/versions/2026.07.08/ docs/constitution/CURRENT \
  autonoetic-types/src/config.rs autonoetic-gateway/src/constitution_glossary_generated.rs
git commit
```

Signer material is unchanged from `2026.07.02` — no `trusted_signers`
update needed unless the key is intentionally rotated.

## Related

- `docs/philosophy.md` — the design commitments this amendment operationalizes.
- `docs/design/principal-model-and-symmetric-obligations.md` (#359) — the
  principal model and symmetric-obligations RFC this amendment draws on for
  §12's bind-direction, `O-6`'s numbering (avoiding the reserved
  `O-3`/`O-4`/`O-5` block), and `I-12`'s Sybil-collapse framing (RFC Part E
  names the horizon this closes a door toward, without building it).
- Baseline: 2026.07.02 (promotion attempt exhaustion gate, #720).
