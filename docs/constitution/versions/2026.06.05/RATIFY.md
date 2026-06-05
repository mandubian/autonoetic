# Ratifying constitution 2026.06.05

This version adds **P-2.25 — Promotion is fail-closed** (§2). It closes a
production fail-open where a new agent was promoted with no operator approval
because the approval requirement was inferred from orchestrator-supplied data.
Enforcement already shipped in PR #419; design:
`docs/design/promotion-completeness-invariant.md`.

> ⚠️ The `gateway-constitution.lock.json` in this directory is a **stale copy**
> of 2026.06.04 — it does **not** match this text. It must be regenerated and
> signed before activation. **The agent never signs the lock; this is the
> operator's step.**

Only **P-2.25** was added vs. 2026.06.04. Rule count: **172 → 173** rules
(rights unchanged at 14). Signer is unchanged
(`autonoetic:constitution:v1`) — no signer rotation, so `trusted_signers` need
not change.

## Operator steps

1. **Review** the diff: the single new row `P-2.25` in §2, and the design doc.

2. **Recompute + sign the lock** (requires PyNaCl and your signing key):
   ```bash
   python3 docs/constitution/recompute_lock.py --version 2026.06.05 \
     --signing-sk-b64 "$AUTONOETIC_CONSTITUTION_SIGNING_SK_B64"
   ```
   This regenerates `2026.06.05/gateway-constitution.lock.json` with the new
   digest + counts and a fresh signature.

3. **Activate** — point the gateway at 2026.06.05 (three places):
   - `autonoetic-types/src/config.rs` — `default_constitution_source_path()` /
     `default_constitution_lock_path()` → `2026.06.05/…`
   - `config/config-template.yaml` — `constitution.source_path` / `lock_path`
     (both the active block and the commented `.gateway/...` variant) → `2026.06.05/…`
   - `docs/config-reference.md` — the two `constitution.*` default cells → `2026.06.05/…`

4. **Map P-2.25 in the enforcement register** (code):
   add a row in `autonoetic-gateway/src/enforcement_register.rs` mapping `P-2.25`
   → its parent principle and the enforcing code+test
   (`runtime/tools/agent_revision.rs` + `constitution_promotion_capability_delta.rs`).

5. **Update version-pinned tests** if any assert the active version string or
   rule/right counts (e.g. `constitution_digest.rs`): bump to `2026.06.05` and
   the 173/14 counts.

6. **Validate:**
   ```bash
   cargo test -p autonoetic-gateway constitution_lock_matches_canonical_digest_and_counts
   cargo test -p autonoetic-gateway --test constitution_r_8_6_retention_policy_startup
   cargo test -p autonoetic-gateway constitution_promotion
   ```

Canonicalization details: `docs/constitution-signing.md`. Multi-machine key
handling: `docs/constitution/key-management.md`.
