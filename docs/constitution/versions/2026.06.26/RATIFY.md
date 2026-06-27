# RATIFY.md — Constitution Version 2026.06.26

## Summary

Constitutional amendment for the **agent-decider runtime** (#204). Promotes the
`GateDecider` capability and its escalation path from `PENDING` to `ENFORCED` in
signed text, and updates the enforcement-citation column to point at the merged
implementation. Baseline: **2026.06.24**. Two clauses amended (no descriptive
text change — only the citation column and status).

This version exists because the `2026.06.24` lock was signed against the
`PENDING` wording; commit `901a96e4` (issue #204) edited `constitution.md`
after signing and intentionally deferred re-signing. Rather than re-sign the
already-ratified `2026.06.24` text, the ENFORCED wording is ratifiable here as
a dated successor, leaving `2026.06.24` intact at its signed state.

**Status at ratification:** the enforcing code is merged (`Capability::GateDecider`
in `autonoetic-types`, `PolicyEngine::can_decide_gate` in `policy.rs`,
`GateService::escalate_to_human` in `runtime/human_gate.rs`, exercised by
`constitution_gate_decider` integration tests). The lock (digest + signature)
must be recomputed with the operator signing key — see "Recompute" below.

## Amendments

### P-2.20 — agent-decider requires GateDecider capability (PENDING → ENFORCED) · #204

Descriptive text unchanged; the enforcement-citation column now names the merged
implementation and the status moves to `ENFORCED`.

```
< | P-2.20 | Agents acting as gate deciders require the `GateDecider` capability. (...) Decider agents are subject to the same dwell time, confirmation phrase, and hardening rules as human operators (P-2.24). | constitution-gate-amendments.md | `scheduler/approval.rs::decide_request_with_options` | PENDING |
---
> | P-2.20 | Agents acting as gate deciders require the `GateDecider` capability. (...) Decider agents are subject to the same dwell time, confirmation phrase, and hardening rules as human operators (P-2.24). | constitution-gate-amendments.md | `autonoetic-types/src/capability.rs::Capability::GateDecider`, `policy.rs::PolicyEngine::can_decide_gate`, `scheduler/approval.rs::decide_request_with_options` | ENFORCED |
```

### P-2.21 — agent-decider must escalate to human on uncertainty (PENDING → ENFORCED) · #204

Descriptive text unchanged; the citation column adds `GateService::escalate_to_human`
and the status moves to `ENFORCED`.

```
< | P-2.21 | When an agent-decider cannot determine whether to approve or reject a gate (...), it must escalate to a human operator rather than reject. Escalation creates a new `GateKind::Escalation` gate referencing the original gate ID. (...) | constitution-gate-amendments.md | `runtime/human_gate.rs::check_escalation` | PENDING |
---
> | P-2.21 | When an agent-decider cannot determine whether to approve or reject a gate (...), it must escalate to a human operator rather than reject. Escalation creates a new `GateKind::Escalation` gate referencing the original gate ID. (...) | constitution-gate-amendments.md | `runtime/human_gate.rs::GateService::escalate_to_human`, `runtime/human_gate.rs::check_escalation` | ENFORCED |
```

## Activation (wired in this change)

- `autonoetic-types/src/config.rs`: `default_constitution_source_path` /
  `default_constitution_lock_path` → `versions/2026.06.26/`.
- `config/config-template.yaml`: `constitution.source_path` / `lock_path` →
  `versions/2026.06.26/`.
- `docs/constitution/CURRENT` → `2026.06.26`.
- `2026.06.24/constitution.md` reverted to its signed state (`d21e3dee`) so the
  `2026.06.24` lock remains valid as a historical baseline.

## Recompute (only if the constitution text or signer material changes)

This version ships a **signed** `gateway-constitution.lock.json` (digest +
Ed25519 signature by `autonoetic:constitution:v1`) — the tests are green and
the gateway boots as-is. Re-running `recompute_lock.py` is only required when:

- the `constitution.md` text is edited (any byte change drifts the digest), or
- the signer key is rotated (`--generate-key`; then also update `trusted_signers`
  in `autonoetic-types/src/config.rs`, `config/config-template.yaml`, and
  `docs/config-reference.md`).

```bash
python3 docs/constitution/recompute_lock.py --version 2026.06.26 \
  --signing-sk-b64 "$AUTONOETIC_CONSTITUTION_SIGNING_SK_B64"
cargo test -p autonoetic-gateway constitution_lock_matches_canonical_digest_and_counts
cargo test -p autonoetic-gateway --test constitution_r_8_6_retention_policy_startup
git add docs/constitution/versions/2026.06.26/gateway-constitution.lock.json && git commit
```

Expected lock values:
- `constitution_digest`: `e546b3e7cfdfaffc59a07f38b034b17719830cb7c39041bb9fe90c40a158576b`
- `rule_enforcement_count`: 176 · `right_enforcement_count`: 16
- `signer_id`: `autonoetic:constitution:v1` (public key
  `lNxT1b/jWa6LqM2Thd7rW1IppvlH3rlEnAOPV81Igzk=` — unchanged from 2026.06.24)

## Related

- Issue: #204 (agent-decider runtime for gate resolution)
- Breaking edit: `901a96e4` (feat(gateway): agent-decider runtime) — edited
  `2026.06.24/constitution.md` after its lock was signed
- Baseline: 2026.06.24 (determinism + divergence)
