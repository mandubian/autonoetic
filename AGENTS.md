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

**Web dashboard dev loop:** the operator dashboard (`web/index.html`) is embedded
into the binary at compile time (`include_str!` in `server/http.rs`). To iterate
on it without rebuilding/restarting, run the gateway with the `dev-web` feature,
which serves the file from disk on every request (browser reload picks up edits):

```bash
cargo run -p autonoetic --features dev-web --bin autonoetic run
```

Plain builds (no features) stay self-contained — always the prod shape.

**Test runner:** CI uses [cargo-nextest](https://nexte.st) (`cargo install cargo-nextest --locked`).
Each test runs in its own process; `.config/nextest.toml` defines two profiles:

```bash
cargo nextest run -p autonoetic-gateway                       # local default: parallel
cargo nextest run -p autonoetic-gateway --profile ci          # serial (CI semantics)
cargo nextest run -E 'test(egress)'                           # filter by name/binary
```

If a suite flakes under the local parallel default but passes with `--profile ci`, it relies on cross-process serialization (ports, fixed paths) — file it against #924.

**Linux toolchain prerequisite:** the repo's `.cargo/config.toml` links with
mold (`-fuse-ld=mold`, scoped to `x86_64-unknown-linux-gnu`). Install it once:

```bash
sudo apt install mold     # Ubuntu 24.04; works with the default GCC driver
```

macOS/Windows checkouts are unaffected (the rustflag is Linux-scoped). The
workspace `Cargo.toml` also sets `debug = "line-tables-only"` for dev/test —
backtraces keep file:line resolution; use a debugger build with full
debuginfo (`CARGO_PROFILE_DEV_DEBUG=2 cargo build`) only when you need
variable inspection.

## Sentinel Baseline Guard

PRs that touch both `sentinel/checks/` and `sentinel/baseline/` are rejected by CI unless a `[baseline-update]` prefix appears in the PR title or a commit message. Check locally:

```bash
cargo xtask sentinel-baseline-guard         # against main
cargo xtask sentinel-baseline-guard my-branch  # against a custom base
```

## Recompute Constitution Lock (Digest + Signature)

When `docs/constitution/versions/<version>/constitution.md` changes, run the
maintained script (requires PyNaCl: `python3 -m pip install pynacl`):

```bash
python3 docs/constitution/recompute_lock.py --version 2026.07.19 \
  --signing-sk-b64 "$AUTONOETIC_CONSTITUTION_SIGNING_SK_B64"
```

To intentionally rotate signer material:

```bash
python3 docs/constitution/recompute_lock.py --version 2026.07.19 --generate-key
```

If you rotate keys, update `trusted_signers` for `autonoetic:constitution:v1`
in:

- `autonoetic-types/src/config.rs`
- `config/config-template.yaml`
- `docs/reference/config.md`

Then validate (note the test targets: the lock test is a `--lib` unit test,
and the constitution suites live in the `constitution` domain binary since the
#922 grouping — there is no `constitution_r_8_6_*` binary anymore):

```bash
cargo test -p autonoetic-gateway --lib constitution_lock_matches_canonical_digest_and_counts
cargo test -p autonoetic-gateway --test constitution r_8_6_retention_policy_startup
```

For activation re-blessing, the register bless test is `bless_register_doc`
(`BLESS_REGISTER=1 cargo test -p autonoetic-gateway --lib bless_register_doc`),
not `bless_enforcement_register`.

Canonicalization details are documented in `docs/constitution/signing.md`.
Operational key handling for multi-machine deterministic signing is in
`docs/constitution/key-management.md`.

## Workspace Structure

```
autonoetic/              CLI binary (clap): gateway, agent, chat, trace commands
autonoetic-gateway/      Core runtime — all logic lives here
  src/
    execution.rs           Session lifecycle, turn execution, tool dispatch, emergency stop
    router.rs              JSON-RPC method routing
    policy.rs              Capability validation before privileged operations
    sandbox.rs             Sandbox orchestration: SDK bridge, dependency composition, spawn/wait
    sandbox/driver/        SandboxDriver trait + registry; one file per backend
                           (bubblewrap, docker, microvm, wasm) — see docs/internals/sandbox/drivers.md
    scheduler/             Approval lifecycle, background reevaluation, gateway SQLite store
    runtime/
      lifecycle.rs         AgentExecutor — the main reasoning loop (the biggest file ~3800 lines)
      tools/               Tool implementations: sandbox, promotion, agent, content, web, etc.
      guard.rs             LoopGuard — per-tool failure budget + max-loops-without-progress
      checkpoint.rs        Session checkpoint serialization
      continuation.rs      Turn continuation (suspend/resume at approval boundaries)
      approved_exec_cache.rs  Fingerprint-based exec replay cache
      remote_access.rs     Static analysis of code for network access patterns
      promotion_store.rs   Promotion record persistence
      response_validation.rs Response contract enforcement
      tools/promotion.rs   promotion.record tool — severity gating (error/critical block pass=true)
      tools/sandbox.rs     sandbox.exec tool — session approval grants, remote access checks
      tools/artifact_prepare.rs artifact.prepare — one-pass credential + approval preflight
      tools/approval.rs      approval.status / approval.withdraw — agent approval management
    runtime/analysis/      Static analysis helpers
  tests/                  50+ integration tests — use tempfile for isolation, serial_test for ordering
autonoetic-types/        Shared types: Agent, Capability, Memory, RuntimeLock, background actions
autonoetic-ofp/          OpenFang Protocol — gateway federation
autonoetic-mcp/          Model Context Protocol integration
agents/                  Agent bundles (SKILL.md manifests)
  lead/                  planner.default
  specialists/           coder, executor, researcher, architect, debugger, evaluator, auditor
  evolution/             specialized_builder, evolution_steward, memory_curator
```

## Architecture Rules

**Separation of Powers**: Agents are low-privilege reasoners; the gateway is the high-privilege executor. Safety-critical invariants are mechanically enforced, never delegated to LLM judgment.

**SQLite (rusqlite)** for transactional data (approvals, sessions, causal events, session approval grants). **Content-addressed storage (SHA-256)** for artifacts. Artifacts are immutable — once created, files never change.

**The GatewayStore** (`scheduler/gateway_store/`) owns the SQLite schema. Migrations are in `migrate.rs` — increment `SCHEMA_VERSION_LATEST` and add a new `apply_*_vN()` function. Each migration checks the current version before running.

**Sandbox host-fs deny-list** (stopgap for #1002): the default bubblewrap driver ro-binds the whole host `/`, so gateway-internal secrets under `<agents_dir>/.gateway` are masked inside the sandbox by `BWRAP_GATEWAY_SENSITIVE_FILES`/`_DIRS` in `sandbox/driver/bubblewrap.rs` (`vault.key`, `gateway.db*`, `state_attestation.ed25519`, `sessions/`, …). **When you add a new secret-bearing file or subdir under `.gateway`, add it to one of those two const lists** or it stays readable from inside any sandboxed exec. The operator config file is masked separately via `sandbox::init_sandbox_host_deny_paths` at startup. The durable fix (explicit mount allow-set) is tracked by #1002.

## Key Patterns

### Adding a new tool
1. Create `autonoetic-gateway/src/runtime/tools/your_tool.rs`
2. Implement the `Tool` trait from `runtime/tools/mod.rs`
3. Register in `runtime/tools/mod.rs` — add to the registry builder
4. Add capability gating in `policy.rs` if the tool is privileged
5. If the tool is universal inert weight — no agent should ever call it (e.g. the workbench console feature) — add its name/prefix to `DEFAULT_EXCLUDED_TOOLS` in `runtime/tools/mod.rs` instead of every agent's `excluded_tools`. There is no per-agent override: `tool_discover` cannot re-enable excluded tools, so the default list must only contain tools with zero legitimate users.

### Adding a new ScheduledAction variant
1. Add the variant to `ScheduledAction` in `autonoetic-types/src/background.rs`
2. Update **all** construction sites and match arms — grep for `ScheduledAction::`
3. If it requires approval, wire it into `scheduler/approval.rs` and `scheduler/decision.rs`
4. Update `LoopGuard` if the checkpoint format changes

### Approval system
Five layers of approval dedup (checked in order):
1. **Exec cache** (fingerprint-level, cross-session) — only when all patterns are concrete (url_literal/ip_address/host_constant); entries expire after `default_grant_ttl_secs` (24h default, 0 disables), same budget as session grants
2. **Plan grants** — operator-approved plan envelope materialized as a session grant; see `docs/reference/capability-grants.md`
3. **Session approval grants** (target-level, scope-aware, within root session) — `session_approval_grants` + `session_approval_grant_targets` tables; supports `ExactHost`, `HostSuffix`, `HostAndPort`, `UrlPrefix`; scoped `RootSession` or `Session`; optional expiry (`expires_at`)
4. **Existing approved/pending approvals** (domain-level matching)
5. **Approval flood cap** (`max_pending_approvals_per_root`, default 50) — rejects requests that would exceed the cap with `approval_flood`

Additional approval features:
- **Grant revocation**: `gateway grants revoke --root-session <id> --host X` without emergency stop; emits `grant_revocation` causal event.
- **Continuation HMAC**: signed with `continuation_key` (or derived from `node_id`); verified on resume; action-equality check vs stored approval.
- **Continuation cleanup**: on resume, reject/cancel/withdraw, gateway startup reaper, emergency stop, task cancellation.

### Anomaly flag flood cap
`anomaly_flag` intake is capability-free (Ri-0.18), so un-adjudicated flags (`pending`/`under_review`) are capped per reporter (`max_pending_anomaly_flags_per_reporter`, default 50, 0 disables) — the #770 spam triage bound, same shape as the P-7.17 approval flood cap. Over-cap filings in `gateway_store/anomaly_flags.rs::insert_anomaly_flag` are rejected loudly with `anomaly_flag_flood` plus an operator notification (deduped per reporter until a filing succeeds); terminal adjudications free capacity.

### Promotion severity gating
`promotion.record` mechanically rejects:
- `pass=true` with any `error` or `critical` finding
- `pass=true` with `warning` findings that lack non-empty `evidence` field

Do NOT add a `warnings_acknowledged` boolean — the LLM will just set it to `true`. The evidence field is the only mechanical proof.

### LoopGuard
Two trip conditions, independent:
1. **Max loops without progress**: `current_loops >= max_loops_without_progress` (default 10). Reset by `register_progress()` (any tool returning `ok: true`).
2. **Per-tool failure budget**: `tool_failure_counts[tool_name] >= max_tool_failures` (default 8). NOT reset by `register_progress()`. Counts total failures per tool name regardless of arguments/hosts.

Both are configurable via `loop_guard:` in `config-template.yaml`.

When modifying `LoopGuard`, update all checkpoint construction sites (grep `loop_guard_state: `).

## Testing

Integration tests use `tempfile::tempdir()` for isolated workspaces and `serial_test::serial` for state isolation. Tests are self-contained — no external services required.

**Grouped test binaries (#922, in progress):** `autonoetic-gateway/tests/` is
being collapsed from ~240 single-file binaries into ~5–10 domain binaries
(`tests/<domain>/main.rs` with one module per former file — done: `egress`).
When adding a new integration suite, put it in the matching domain binary
rather than creating a new top-level `tests/*.rs` file. Suites that bind
ports, use fixed paths, or spawn singleton daemons must stay in their own
binary (or be refactored to `tempfile` + port-0 first) — cohabiting one
process requires no cross-test external state.

**Where a test lives (#1001):** pure module semantics stay as `#[cfg(test)]`
unit tests next to the code (`scheduler/gateway_store/workspace_taint.rs::tests`,
`runtime/egress_labeler.rs::tests` — in-memory stores, no injection). Contract
tests — invariants that combine store + labeler + routing, and anything needing
failure injection against a real store (dropped table, corrupted row, …) — go
in the domain binary (`tests/egress/`, one module per subject, e.g.
`tests/egress/workspace_taint.rs`), not the crate's unit modules. When in
doubt, the domain binary is the default.

**Enforcement-register citation coupling:** the constitution's enforcement
register cites test files by name, and `enforcement_register.rs`'s
`every_parseable_citation_resolves` test fails the build when a cited file
doesn't exist. A rename/move must update **both** copies of the register
(`docs/constitution/enforcement-register.md` and the `EnforcementEntry`
structs in `autonoetic-gateway/src/enforcement_register.rs`) — cite the new
path form (`promotion/attempt_exhaustion.rs` resolves under
`autonoetic-gateway/tests/`). Bare-filename citations still resolve
recursively, so keeping the basename unchanged also works.

Notable test suites:
- `turn_continuation_approval_integration.rs` — suspend/resume, timeout, cancellation, restart, parallel-join
- `approved_exec_cache_integration.rs` — cache fingerprint, normalization, full cycle
- `emergency_stop_root_session_integration.rs` — circuit breaker, grant cleanup
- `promotion/record_e2e.rs` / `promotion/gate_hardening.rs` — severity gating
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
- `docs/wiki/approval-system.md` — Approval lifecycle, session grants, dedup, escalation (maintained digest; the pre-unification write-up is `docs/archived/approval-system.md`)
- `docs/internals/approval-cache.md` — Exec replay cache and session approval grants
- `docs/reference/capability-grants.md` — Plan-as-capability-grant: materialization, revocation, dedup layer
- `docs/guide/remote-access-approval.md` — Static analysis detection, approval flow diagram
- `docs/reference/credentials.md` — Credential vault, `credential_env` injection, CLI credential commands
- `docs/AGENTS.md` — Agent roles, SKILL.md format, capabilities (user-facing, not dev instructions)
