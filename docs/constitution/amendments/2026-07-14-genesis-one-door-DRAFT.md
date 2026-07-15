# DRAFT amendment — Agent genesis: single door (P-9.15), import provenance (P-9.16), creation-is-not-delegation (I-13)

> **Status: DRAFT — NOT LAW.** Ready-to-apply package; binds no one until a
> new constitution version carries it and the lock is recomputed and
> **signed**. Implements the "Agent Genesis — One Door" RFC
> (`docs/design/agent-genesis-one-door.md`). **Sign together with** the
> pending batch (`2026-07-12-anomaly-reporting-DRAFT.md`,
> `2026-07-12-adjudication-sla-DRAFT.md`).
>
> Context: a design audit (2026-07-14) found `skill_install` reached full
> agent activation while bypassing every §9 gate, unmentioned by the
> constitution; and that no import provenance was recorded anywhere. The
> mechanisms below ship in code ahead of enactment (the RFC's fix-now PR);
> these clauses make the door and the provenance *law* rather than current
> behavior.

## Clause text

### Insert into §9 Agent Install & Provenance, after the P-9.14 row:

```markdown
| P-9.15 | **Single door.** Every surface that activates an agent — moves an alias to a revision — passes the same promotion gates: eval gating (P-9.7), high-risk capability review (P-9.9), and the capability-delta approval (P-2.25). No installation convenience (remote skill import, factory pipeline, RPC, CLI) may bypass them. Sole exception: gateway-startup bootstrap of the local repository's own reference bundles, which installs the operator's own code before any session exists. | `runtime/tools/skill.rs::skill_install` installs as **Candidate** only (never promotes); activation flows through `agent_revision_promote`'s gate matrix; startup bootstrap's auto-promote is parameter-explicit in `bootstrap.rs`. Pinned by the one-door test (skill_install result carries `activated: false` and the revision is Candidate). | ENFORCED |
| P-9.16 | **Import provenance.** An agent installed from an external source durably records, on its revision, the source URL, a content digest of the fetched material, and the install time (`source_kind: "skill_install"`, `source_ref: "<url>#sha256=<digest>"`), and the install emits a causal event. What arrived from outside, from where, and when is a permanent, queryable fact — a distinction that is conceivable at install time and unrecorded can never be back-filled (philosophy §4.7). | `runtime/tools/skill.rs` provenance recording + `agent_install`/`skill_imported` causal event; pinned by the import-provenance test. | ENFORCED |
```

### Insert into §13 Cross-cutting invariants, after I-12:

```markdown
- **I-13 (Creation is not delegation.)** A newborn agent's capabilities are
  granted through the promotion gate — evidence plus the approved capability
  delta (P-2.25) — and are neither inherited from, nor bounded by, the
  creating agent's own capabilities. Hereditary capability bounds would
  freeze the community to its founders' powers; the gate, not the lineage,
  is the source of authority. (Declared invariant documenting an existing
  deliberate absence: no attenuation check exists, by design.)
```

## Enforcement-register additions (at enactment)

```rust
// In principles() (or the §9 rules structure the register uses):
// P-9.15
EnforcementEntry {
    clause_id: "P-9",
    rule_id: "P-9.15",
    check_id: "single_door_activation",
    code: "runtime/tools/skill.rs::skill_install installs Candidate only; bootstrap.rs auto-promote is parameter-explicit (startup exception)",
    test: "<one-door test name from the fix-now PR>",
    config: None,
},
// P-9.16
EnforcementEntry {
    clause_id: "P-9",
    rule_id: "P-9.16",
    check_id: "import_provenance_recorded",
    code: "runtime/tools/skill.rs source_kind/source_ref + agent_install/skill_imported causal event",
    test: "<import-provenance test name from the fix-now PR>",
    config: None,
},
```

(Reconcile `clause_id`/shape with how existing P-9.x entries are registered,
and fill the `test:` names from the merged fix-now PR. I-13 is a declared
invariant like I-12 — register only if invariants carry entries in the
current register shape.)

## Enactment checklist (on the signing machine)

1. Apply the §9 rows and the I-13 bullet to the new constitution version's
   `constitution.md`, alongside the two pending 2026-07-12 drafts.
2. Recompute + sign the lock (`recompute_lock.py --version <new-date>`);
   rule-count rises by 2 (P-9.15, P-9.16).
3. Register additions above; `BLESS_REGISTER=1` regeneration; text-presence
   pins for the new clauses.
4. Validate lock + register test modules; delete this draft.
