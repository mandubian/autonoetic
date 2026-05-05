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
`<agents_dir>/.gateway/constitution/` with:

- `CURRENT`
- `ACTIVE.json`
- `versions/<version>/constitution.md`
- `versions/<version>/gateway-constitution.lock.json`

Lock signatures:

- release locks are signed by trusted signer IDs from
  `constitution.trusted_signers` (for example `autonoetic:constitution:v1`),
- bootstrapped `.gateway` locks are re-signed by the local gateway identity
  (`gateway:<fingerprint>`), and verified against
  `<agents_dir>/.gateway/state_attestation.ed25519.pub`.

Precise signature payload and verification rules are specified in:

- `docs/constitution-signing.md`
- `docs/constitution/key-management.md`

Each constitutional release should:

1. add/update `docs/constitution/versions/<version>/constitution.md`,
2. add/update `docs/constitution/versions/<version>/gateway-constitution.lock.json`,
3. ensure the lock signature matches the lock payload (`docs/constitution-signing.md`),
4. update `docs/constitution/CURRENT`,
5. update `config/config-template.yaml` defaults when promoting the new release.
