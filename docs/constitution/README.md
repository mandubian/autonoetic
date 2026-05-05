# Constitution Version Tree

This directory keeps a human-readable version tree for constitutional
releases.

## Layout

- `CURRENT` - active constitution version identifier.
- `versions/<version>/constitution.md` - human snapshot entry for that version.
- `versions/<version>/gateway-constitution.lock.json` - canonical digest lock for that version.

## Active Canonical Source

The active canonical markdown text remains at `docs/gateway-constitution.md`
for backward compatibility with existing links and tooling.

Each constitutional release should:

1. update `docs/gateway-constitution.md`,
2. update `docs/constitution/versions/<version>/gateway-constitution.lock.json`,
3. update `docs/constitution/CURRENT`.
