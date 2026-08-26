# Constitution Version Tree

This directory keeps a human-readable version tree for constitutional
releases.

## Layout

- `CURRENT` - active constitution version identifier.
- `versions/<version>/constitution.md` - human snapshot entry for that version.
- `versions/<version>/gateway-constitution.lock.json` - canonical digest lock for that version.

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

The docs reorganisation (`docs/proposals/docs-reorganization.md`) moved four
docs that the signed texts cite:

| Cited in signed versions | Now at |
|---|---|
| `docs/philosophy.md` | [`../concepts/philosophy.md`](../concepts/philosophy.md) |
| `docs/config-reference.md` | [`../reference/config.md`](../reference/config.md) |
| `docs/gateway-constitution-roadmap.md` | [`roadmap.md`](roadmap.md) |
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
