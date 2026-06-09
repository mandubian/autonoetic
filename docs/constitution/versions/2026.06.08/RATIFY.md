# Ratifying constitution 2026.06.08

This version adds **P-2.26 — All executed gate roles must pass** (§2). It
closes a gap where the promotion gate only checked evaluator and auditor
passes (P-2.8) but silently ignored the `unit_test_runner` verdict.
Enforcement already shipped in this PR; design:
`docs/design/promotion-completeness-invariant.md`.

> ⚠️ The `gateway-constitution.lock.json` in this directory was regenerated
> and signed as part of this PR. Before activation, **verify** the lock
> matches the canonical digest and counts (step 6) — a re-sign is only
> needed if verification fails. **The agent never signs the lock; this is
> the operator's step.**

Only **P-2.26** was added vs. 2026.06.05. Rule count: **173 → 174** rules
(rights unchanged at 14). Signer is unchanged
(`autonoetic:constitution:v1`) — no signer rotation, so `trusted_signers` need
not change.

## Operator steps

1. **Review** the diff: the single new row `P-2.26` in §2, and the design doc.

2. **Verify the lock** — the file was signed during the PR:
   ```bash
   cargo test -p autonoetic-gateway constitution_lock_matches_canonical_digest_and_counts
   ```
   If verification fails, **recompute + sign** (requires PyNaCl and your signing key):
   ```bash
   python3 docs/constitution/recompute_lock.py --version 2026.06.08 \
     --signing-sk-b64 "$AUTONOETIC_CONSTITUTION_SIGNING_SK_B64"
   ```
   Re-signing regenerates `2026.06.08/gateway-constitution.lock.json` with the
   correct digest + counts and a fresh signature.

3. **Activate** — point the gateway at 2026.06.08 (three places):
   - `autonoetic-types/src/config.rs` — `default_constitution_source_path()` /
     `default_constitution_lock_path()` → `2026.06.08/…`
   - `config/config-template.yaml` — `constitution.source_path` / `lock_path`
     (both the active block and the commented `.gateway/...` variant) → `2026.06.08/…`
   - `docs/config-reference.md` — the two `constitution.*` default cells → `2026.06.08/…`

4. **Map P-2.26 in the enforcement register** (code):
   add a row in `autonoetic-gateway/src/enforcement_register.rs` mapping `P-2.26`
   → its parent principle and the enforcing code+test
   (`runtime/tools/agent_revision.rs` + `constitution_p_2_26_unit_test_runner_gate.rs`).

5. **Update version-pinned tests** if any assert the active version string or
   rule/right counts (e.g. `constitution_digest.rs`): bump to `2026.06.08` and
   the 174/14 counts.

6. **Validate:**
   ```bash
   cargo test -p autonoetic-gateway constitution_lock_matches_canonical_digest_and_counts
   cargo test -p autonoetic-gateway --test constitution_r_8_6_retention_policy_startup
   cargo test -p autonoetic-gateway \
     --test constitution_p_2_26_unit_test_runner_gate
   ```

Canonicalization details: `docs/constitution-signing.md`. Multi-machine key
handling: `docs/constitution/key-management.md`.
