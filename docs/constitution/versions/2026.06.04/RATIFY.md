# 2026.06.04 — prepared, UNSIGNED (awaits ratification)

This version adds **§O. Decider Obligations** (`O-1` graduated motivation, `O-2`
attribution) — the symmetric-obligation amendment (#359 / #395). It is
**prepared but not ratified**: the active constitution remains `2026.06.02`
(see `config.constitution.source_path`), so the gateway is unaffected until the
configured authority signs and switches over.

The enforcement *mechanism* already ships (PR #396); this formalizes the *law*.

## Ratification steps (configured authority only — the agent never signs)

1. Recompute the digest + sign the lock for this version:
   ```bash
   python3 docs/constitution/recompute_lock.py --version 2026.06.04 \
     --signing-sk-b64 "$AUTONOETIC_CONSTITUTION_SIGNING_SK_B64"
   ```
   (The `gateway-constitution.lock.json` here is a copy of 2026.06.02's and has
   a **stale digest** until this runs — it will not validate as-is.)
2. Point the active constitution at this version:
   - `config.constitution.source_path` → `docs/constitution/versions/2026.06.04/constitution.md`
   - `config.constitution.lock_path`   → `docs/constitution/versions/2026.06.04/gateway-constitution.lock.json`
   (defaults live in `autonoetic-types/src/config.rs`).
3. Add the `O-*` rows to `autonoetic-gateway/src/enforcement_register.rs` so
   contract-health attributes `O-1`/`O-2` enforcement events to their clause
   (do this together with the switch-over so rule counts stay consistent).
4. Validate:
   ```bash
   cargo test -p autonoetic-gateway constitution_lock_matches_canonical_digest_and_counts
   cargo test -p autonoetic-gateway --test constitution_o_1_decider_motivation
   ```

Until step 1–2 are done, nothing changes: 2026.06.02 stays the law of the land.
