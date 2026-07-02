# RATIFY.md — Constitution Version 2026.07.02

## Summary

Constitutional amendment for the **promotion attempt exhaustion gate** (#720).
Adds rule `P-2.29` to the signed text: too many rejected promotion attempts for
the same `(alias, content_digest)` across sessions blocks further attempts until
an operator acknowledges the revision (via an approved `RevisionPromote`).

Baseline: **2026.06.26**. One rule added.

## Amendment

### P-2.29 — Promotion attempt exhaustion gate · #720

```
| P-2.29 | **Promotion attempt exhaustion gate.** Too many rejected promotion attempts for the same `(alias, content_digest)` across sessions blocks further attempts until an operator acknowledges the revision. | this amendment | `runtime/promotion_governor.rs::check_attempt_exhaustion`, `runtime/tools/agent_revision.rs::record_attempt`, `gateway_store/agent_registry.rs::promotion_attempts` | ENFORCED |
```

## Activation (wired in this change)

- `autonoetic-types/src/config.rs`: `ACTIVE_CONSTITUTION_VERSION` → `2026.07.02`;
  `default_constitution_source_path` / `default_constitution_lock_path` updated;
  trusted signer public key updated to the newly generated key.
- `config/config-template.yaml`: `trusted_signers` public key updated.
- `docs/config-reference.md`: `trusted_signers` public key updated.
- `docs/constitution/CURRENT` → `2026.07.02`.

## Recompute (only if the constitution text or signer material changes)

This version ships a **placeholder-signed** `gateway-constitution.lock.json`
(digest + Ed25519 signature generated with `--generate-key`). The lock must be
recomputed with the operator signing key before the gateway is run in
production:

```bash
python3 docs/constitution/recompute_lock.py --version 2026.07.02 \
  --signing-sk-b64 "$AUTONOETIC_CONSTITUTION_SIGNING_SK_B64"
cargo test -p autonoetic-gateway constitution_lock_matches_canonical_digest_and_counts
cargo test -p autonoetic-gateway --test constitution_r_8_6_retention_policy_startup
git add docs/constitution/versions/2026.07.02/gateway-constitution.lock.json && git commit
```

If you recompute with a different signer, also update `trusted_signers` for
`autonoetic:constitution:v1` in:

- `autonoetic-types/src/config.rs`
- `config/config-template.yaml`
- `docs/config-reference.md`

Expected lock values (as signed with the existing `autonoetic:constitution:v1` key):
- `constitution_digest`: `3ed36657394b9d3200315b9490c37353776b972b83c66d067c2f646e25cfe5a5`
- `rule_enforcement_count`: 177 · `right_enforcement_count`: 16
- `signer_id`: `autonoetic:constitution:v1` (public key
  `lNxT1b/jWa6LqM2Thd7rW1IppvlH3rlEnAOPV81Igzk=` — unchanged from 2026.06.26)

## Related

- Issue: #720 (durable cross-session promotion-attempt ledger)
- Baseline: 2026.06.26 (agent-decider runtime)
