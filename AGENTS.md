# AGENTS.md

Developer instruction file for OpenCode sessions working on this repository.

## Build & Test

```bash
cargo build                                    # Build all workspace crates
cargo build -p autonoetic-gateway              # Build a single crate
cargo test                                     # Run all tests
cargo test -p autonoetic-gateway               # Gateway crate only (most tests live here)
cargo test --test turn_continuation_approval   # Run a single integration test file
RUST_LOG=autonoetic=debug cargo test           # Debug logging during tests
```

No linter or formatter is configured. There is no `cargo clippy` or `rustfmt` CI gate — run `cargo build` to verify.

Notable test suites:
- `turn_continuation_approval_integration.rs` — suspend/resume, timeout, cancellation, restart, parallel-join
- `approved_exec_cache_integration.rs` — cache fingerprint, normalization, full cycle
- `emergency_stop_root_session_integration.rs` — circuit breaker, grant cleanup
- `promotion_record_e2e.rs` / `promotion_gate_hardening_integration.rs` — severity gating
- `continuation_hmac_integrity_integration.rs` — HMAC signing, verification, tamper detection
- `continuation_cleanup_integration.rs` — delete on reject/cancel/withdraw, startup reaper, emergency stop
- `approval_scope_targets_integration.rs` — session-scoped grants, pattern-based targets, expiry
- `approval_grant_revocation_integration.rs` — revoke all/specific host, causal event
- `plan_frame_integration.rs` — plan approval grant materialization, amend revoke, inherit
- `constitution_abuse_approval_flood.rs` — flood cap enforcement, cap=0 bypass

## SDKs

SDKs live outside the Rust workspace:
- `autonoetic-sdk/python/` — Python SDK (JSON-RPC over Unix socket or HTTP)
- `autonoetic-sdk/typescript/` — TypeScript SDK: `cd autonoetic-sdk/typescript && npm run build`

## Docs

- `docs/ARCHITECTURE.md` — System design, security model, data flow, emergency stop
- `docs/approval-system.md` — Full approval lifecycle, session grants, promotion gating
- `docs/plan-capability-grants.md` — Plan-as-capability-grant: materialization, revocation, dedup layer
- `docs/remote-access-approval.md` — Static analysis detection, approval flow diagram
- `docs/credential-management.md` — Credential vault, `credential_env` injection, CLI credential commands
- `docs/AGENTS.md` — Agent roles, SKILL.md format, capabilities (user-facing, not dev instructions)
