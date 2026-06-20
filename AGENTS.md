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
python3 docs/constitution/recompute_lock.py --version 2026.05.30 \
  --signing-sk-b64 "$AUTONOETIC_CONSTITUTION_SIGNING_SK_B64"
```

To intentionally rotate signer material:

```bash
python3 docs/constitution/recompute_lock.py --version 2026.05.30 --generate-key
```

If you rotate keys, update `trusted_signers` for `autonoetic:constitution:v1`
in:

- `autonoetic-types/src/config.rs`
- `config/config-template.yaml`
- `docs/config-reference.md`

Then validate:

```bash
cargo test -p autonoetic-gateway constitution_lock_matches_canonical_digest_and_counts
cargo test -p autonoetic-gateway --test constitution_r_8_6_retention_policy_startup
```

Canonicalization details are documented in `docs/constitution-signing.md`.
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
    sandbox.rs             Sandbox drivers (bubblewrap, docker, microvm)
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

## Key Patterns

### Adding a new tool
1. Create `autonoetic-gateway/src/runtime/tools/your_tool.rs`
2. Implement the `Tool` trait from `runtime/tools/mod.rs`
3. Register in `runtime/tools/mod.rs` — add to the registry builder
4. Add capability gating in `policy.rs` if the tool is privileged

### Adding a new ScheduledAction variant
1. Add the variant to `ScheduledAction` in `autonoetic-types/src/background.rs`
2. Update **all** construction sites and match arms — grep for `ScheduledAction::`
3. If it requires approval, wire it into `scheduler/approval.rs` and `scheduler/decision.rs`
4. Update `LoopGuardState` if the checkpoint format changes

### Approval system
Five layers of approval dedup (checked in order):
1. **Exec cache** (fingerprint-level, cross-session) — only when all patterns are concrete (url_literal/ip_address)
2. **Plan grants** — operator-approved plan envelope materialized as a session grant; see `docs/plan-capability-grants.md`
3. **Session approval grants** (target-level, scope-aware, within root session) — `session_approval_grants` + `session_approval_grant_targets` tables; supports `ExactHost`, `HostSuffix`, `HostAndPort`, `UrlPrefix`; scoped `RootSession` or `Session`; optional expiry (`expires_at`)
4. **Existing approved/pending approvals** (domain-level matching)
5. **Approval flood cap** (`max_pending_approvals_per_root`, default 50) — rejects requests that would exceed the cap with `approval_flood`

Additional approval features:
- **Similarity scoring**: on creation, Jaccard similarity over command tokens (70%) + hosts (30%) against recent same-agent approvals. Stored as `similar_to_request_id` + `similarity_score`.
- **Grant revocation**: `gateway grants revoke --root-session <id> --host X` without emergency stop; emits `grant_revocation` causal event.
- **Continuation HMAC**: signed with `continuation_key` (or derived from `node_id`); verified on resume; action-equality check vs stored approval.
- **Continuation cleanup**: on resume, reject/cancel/withdraw, gateway startup reaper, emergency stop, task cancellation.

### Promotion severity gating
`promotion.record` mechanically rejects:
- `pass=true` with any `error` or `critical` finding
- `pass=true` with `warning` findings that lack non-empty `evidence` field

Do NOT add a `warnings_acknowledged` boolean — the LLM will just set it to `true`. The evidence field is the only mechanical proof.

### LoopGuard
Two trip conditions, independent:
1. **Max loops without progress**: `current_loops >= max_loops_without_progress` (default 5). Reset by `register_progress()` (any tool returning `ok: true`).
2. **Per-tool failure budget**: `tool_failure_counts[tool_name] >= max_tool_failures` (default 5). NOT reset by `register_progress()`. Counts total failures per tool name regardless of arguments/hosts.

Both are configurable via `loop_guard:` in `config-template.yaml`.

When modifying `LoopGuardState`, update all checkpoint construction sites (grep `LoopGuardState {`).

## Testing

Integration tests use `tempfile::tempdir()` for isolated workspaces and `serial_test::serial` for state isolation. Tests are self-contained — no external services required.

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

## Anchored Summary — Post-decision Bookkeeping (PR #562) in progress

Working in worktree: `../autonoetic.worktrees/post-decision-bookkeeping` on branch `refactor/post-decision-bookkeeping`

**Done**:
- `apply_decision` defined — single fan-out for all post-decision side-effects (session grants, escalation resolve, reeval/background state, signal write, wiki timeline, causal event, task unblock)
- `DecisionContext` struct for pre-computed metadata (wiki_materialized_meta)
- `resume_session_after_approval` deleted (~286 lines, including 130-line string ladder) — replaced by `notify_session_of_decision` with simplified message format
- `emit_wiki_timeline_event` deleted — replaced by `emit_wiki_decision_event` emitting normalized `wiki.decision` event with `decision: "approved"|"rejected"|"cancelled"|"withdrawn"` field
- All 5 entry points route through `apply_decision`: approve, reject, cancel, cancel-for-task, withdraw(tool)
- Side-effects stripped from `decide_request_with_options` and `cancel_approval_request` (moved to `apply_decision`)
- Policy decisions documented in `apply_decision` docstring (agent withdrawal, cancellation, escalation, wiki, causal events)
- `cancel_approval_request` made `pub(crate)` for tool use
- 1355 tests pass (2 pre-existing unrelated failures in `agent_adapter_wrapper_integration`)

**Next**: Create PR and merge. Then proceed to #563 (collapse approval dedup onto GateService) or #566 (extract pre_turn_checks/handle_tool_batch).
