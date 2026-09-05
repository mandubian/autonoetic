# Constitution Version Tree

This directory keeps a human-readable version tree for constitutional
releases.

## Layout

- `CURRENT` - active constitution version identifier.
- `versions/<version>/constitution.md` - human snapshot entry for that version.
- `versions/<version>/gateway-constitution.lock.json` - canonical digest lock for that version.

Two **generated** views over the active version, answering different questions.
Both carry a drift guard, so neither can quietly disagree with its generator:

- [`law-table.md`](law-table.md) — **the law.** One row per clause: which power
  it **binds**, who has standing to **invoke** it, and its verification field.
  Identical for any implementation, so this is what a re-implementer reads.
  Covers every declared clause, including the ones no code enforces
  (`U-1`–`U-3` are `MISSING` and appear nowhere else).
  Regenerate: `BLESS_LAW_TABLE=1 cargo test -p autonoetic-gateway --lib bless_law_table`
- [`enforcement-register.md`](enforcement-register.md) — **the conformance
  record.** Per clause, *this gateway's* code sites, tests and config knobs.
  A different implementation replaces this and inherits the law table.
  Regenerate: `BLESS_REGISTER=1 cargo test -p autonoetic-gateway --lib bless_register_doc`

## Active Canonical Source

The active canonical markdown text is:

- `docs/constitution/versions/<CURRENT>/constitution.md`

The gateway enforces whichever source/lock paths are configured in
`config.yaml` under:

- `constitution.source_path`
- `constitution.lock_path`

At runtime, the gateway also bootstraps a local constitution snapshot under
`<runtime_dir>/constitution/` with:

- `CURRENT`
- `ACTIVE.json`
- `versions/<version>/constitution.md`
- `versions/<version>/gateway-constitution.lock.json`

Lock signatures:

- release locks are signed by trusted signer IDs from
  `constitution.trusted_signers` (for example `autonoetic:constitution:v1`),
- bootstrapped runtime-dir locks are re-signed by the local gateway identity
  (`gateway:<fingerprint>`), and verified against
  `<runtime_dir>/state_attestation.ed25519.pub`.

Precise signature payload and verification rules are specified in:

- `docs/constitution/signing.md`
- `docs/constitution/key-management.md`

## Doc paths cited by signed versions

A ratified `constitution.md` is **digest-signed**: its bytes cannot be edited
without invalidating the lock. So when the surrounding documentation is
reorganised, citations inside already-signed versions keep pointing at the old
paths — permanently, by design. They are historical artifacts, not stale files.

The docs reorganisation (`docs/archived/docs-reorganization.md`) moved four
docs that the signed texts cite:

| Cited in signed versions | Now at |
|---|---|
| `docs/philosophy.md` | [`../concepts/philosophy.md`](../concepts/philosophy.md) |
| `docs/config-reference.md` | [`../reference/config.md`](../reference/config.md) |
| `docs/gateway-constitution-roadmap.md` | [`../archived/gateway-constitution-roadmap.md`](../archived/gateway-constitution-roadmap.md) — plan complete, archived 2026-09-02 |
| `docs/gateway-constitution-audit-2026-04-24.md` | [`../reports/2026-04-24-constitution-audit.md`](../reports/2026-04-24-constitution-audit.md) |

The enforcement tables also name two docs by bare filename in their **Doc**
column (P-6.1…P-6.5), and those two have since been merged into one:

| Named in the Doc column | Now |
|---|---|
| `session-budget.md` | merged into [`../reference/budgets.md`](../reference/budgets.md) |
| `budget-management.md` | merged into [`../reference/budgets.md`](../reference/budgets.md) |

The next ratified version should cite the new paths in its own text; until then
this table is the mapping. `docs_link_guard` deliberately does not scan
`versions/**` for exactly this reason — a guard that "fixed" a signed text
would break its signature.

Each constitutional release should:

1. add/update `docs/constitution/versions/<version>/constitution.md`,
2. add/update `docs/constitution/versions/<version>/gateway-constitution.lock.json`,
3. ensure the lock signature matches the lock payload (`docs/constitution/signing.md`),
4. update `docs/constitution/CURRENT`,
5. update `config/config-template.yaml` defaults when promoting the new release.

### The amendment materializer (#810)

Step 1 no longer has to be a hand edit. Approved Ri-0.8 proposals can be
mechanically drafted into a candidate version:

```bash
autonoetic gateway constitution materialize [--version YYYY.MM.DD] [PROPOSAL_ID...]
```

This applies each approved-but-unmaterialized proposal to a copy of the
active constitution (modify = replace the statement cell; remove = delete the
row; add = insert after the clause's section siblings with explicit `DRAFT`
placeholder cells that the operator completes substantively), writes
`versions/<candidate>/` with the markdown, an **unsigned** lock whose digest
is computed through the same canonicalization `recompute_lock.py` signs, and
a `provenance.json` linking every proposal to its adjudication and a
before/after row diff. The candidate directory is inert — the gateway drafts,
never enacts: `CURRENT`, the active-version pin, and every signed byte stay
untouched until the operator reviews, signs, and activates through the
ceremony above.
