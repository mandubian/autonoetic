# Ratifying constitution 2026.06.21

This is a **mechanical-citation update** only. No new rights, principles, or
rules are added, and no rule text is changed. It reflects the gateway
simplification work completed under issue #561 (#564–#571, #573–#574):
`TurnContinuation` collapsed into `SessionCheckpoint`, `continuation.rs`
removed, and close-reason handling unified under `SessionCloseOutcome`.

The following enforcement citations are updated to point at the current code:

| Clause | Old citation | New citation |
|---|---|---|
| Ri-0.12 | `ExecuteLoopTermination` + `finalize_execute_loop_result` | `autonoetic_types::session_outcome::SessionCloseOutcome` |
| §2 intro | Replay on approve via `runtime/continuation.rs` | Replay on approve via `runtime/checkpoint.rs::SessionCheckpoint` |
| P-2.10 | `runtime/continuation.rs:178 execute_approved_action`, `execution.rs::resume_from_checkpoint` | `runtime/checkpoint.rs::SessionCheckpoint` + `execution.rs::resume_from_checkpoint` |
| P-2.15 | `continuation.rs:332` | `runtime/checkpoint.rs::SessionCheckpoint` |
| P-6.15 | `runtime/continuation.rs` | `runtime/checkpoint.rs::SessionCheckpoint` |
| P-11.8 | `TurnContinuation` storage | `SessionCheckpoint` storage |

The amendment-process self-reference at the end of the document now points to
`docs/constitution/versions/2026.06.21/gateway-constitution.lock.json`.

> ⚠️ **The `gateway-constitution.lock.json` in this directory is intentionally
> absent.** It must be generated and signed by the maintainer after reviewing
> this diff. The agent never signs the lock; this is the operator's step.

Rule count is unchanged: **174 rules**, **14 rights** (same as 2026.06.16).
Signer is unchanged (`autonoetic:constitution:v1`) — no signer rotation, so
`trusted_signers` need not change.

## Operator steps

1. **Review** the diff above — only enforcement citations changed; no rule text
   or rights changed.

2. **Generate and sign the lock** (requires PyNaCl and your signing key):
   ```bash
   python3 docs/constitution/recompute_lock.py --version 2026.06.21 \
     --signing-sk-b64 "$AUTONOETIC_CONSTITUTION_SIGNING_SK_B64"
   ```
   This creates `2026.06.21/gateway-constitution.lock.json` with the correct
   digest + counts and a fresh signature.

3. **Verify the lock**:
   ```bash
   cargo test -p autonoetic-gateway constitution_lock_matches_canonical_digest_and_counts
   ```

4. **Activate** — point the gateway at 2026.06.21 (three places):
   - `autonoetic-types/src/config.rs` — `default_constitution_source_path()` /
     `default_constitution_lock_path()` → `2026.06.21/…`
   - `config/config-template.yaml` — `constitution.source_path` / `lock_path`
     (both the active block and the commented `runtime/...` variant) → `2026.06.21/…`
   - `docs/config-reference.md` — the two `constitution.*` default cells → `2026.06.21/…`

5. **Validate:**
   ```bash
   cargo test -p autonoetic-gateway constitution_lock_matches_canonical_digest_and_counts
   cargo test -p autonoetic-gateway --test constitution_r_8_6_retention_policy_startup
   ```

Canonicalization details: `docs/constitution-signing.md`. Multi-machine key
handling: `docs/constitution/key-management.md`.
