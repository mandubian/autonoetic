# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

### Build
```bash
cargo build                          # Build all workspace crates
cargo build -p autonoetic-gateway   # Build a specific crate
cargo build --release                # Release build
```

### Test
```bash
cargo test                                          # Run all tests
cargo test --test agent_install_approval_e2e        # Run a single integration test
cargo test --test full_lifecycle_integration        # Run lifecycle integration test
RUST_LOG=autonoetic=debug cargo test               # Run with debug logging
```

### Run
```bash
cargo run -p autonoetic -- gateway start           # Start the gateway daemon
cargo run -p autonoetic -- agent list              # List installed agents
cargo run -p autonoetic -- chat <agent_id>         # Interactive chat with an agent
cargo run -p autonoetic -- trace list              # List session traces
bash examples/quickstart/run.sh                    # Run quickstart example
```

### TypeScript SDK
```bash
cd autonoetic-sdk/typescript && npm run build      # Build TS SDK
```

### Recompute Constitution Lock (Digest + Signature)
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

If signer key rotated, update `trusted_signers` for
`autonoetic:constitution:v1` in:

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

## Architecture

Autonoetic is a Rust runtime for autonomous agents with durable memory, portable identity, and reproducible execution. It enforces a strict **Separation of Powers**: agents are low-privilege reasoners that propose intents; the Gateway is the high-privilege executor that validates and runs them.

### Workspace Crates

| Crate | Role |
|---|---|
| `autonoetic` | CLI binary (clap) — commands: `gateway`, `agent`, `chat`, `trace` |
| `autonoetic-gateway` | Core runtime: execution engine, policy engine, artifact store, causal chain, sandbox, scheduler, HTTP API |
| `autonoetic-types` | Shared type definitions (Agent, Capability, Memory, RuntimeLock, etc.) |
| `autonoetic-ofp` | OpenFang Protocol — federation between gateway nodes |
| `autonoetic-mcp` | Model Context Protocol — tool/capability discovery integration |

SDKs live outside the Rust workspace:
- `autonoetic-sdk/python/` — Python SDK (JSON-RPC over Unix socket or HTTP)
- `autonoetic-sdk/typescript/` — TypeScript SDK (mirrors Python API)

### Gateway Internals (`autonoetic-gateway/src/`)

Key files:
- `execution.rs` — Agent session lifecycle, turn execution, tool dispatch
- `router.rs` — JSON-RPC method routing for all SDK calls
- `policy.rs` — Capability validation before any privileged operation
- `sandbox.rs` — Sandbox drivers: bubblewrap, docker, microvm
- `runtime/tools.rs` — Tool definitions exposed to agents
- `runtime/lifecycle.rs` — Session management and state transitions

Storage uses SQLite (via `rusqlite`) for transactional data and a content-addressed store (SHA-256) for artifacts.

### Agent Model

Agents are defined by `SKILL.md` manifests with YAML frontmatter. The `metadata.autonoetic` section contains the Autonoetic-specific runtime config (LLM provider/model, capabilities, sandbox type, max children, etc.).

Reference bundles are under `agents/`:
- `agents/lead/` — Orchestration (planner)
- `agents/specialists/` — Coder, researcher, architect, debugger, evaluator, auditor
- `agents/evolution/` — Specialized builder, evolution steward, memory curator

### Key Concepts

- **Causal Chain**: Every session produces a hash-chained audit trail of turns and events
- **Checkpoint**: Universal session snapshots at every yield point (hibernation, approval, budget exhaustion, emergency stop). Enables crash recovery and session forking.
- **Queryable Event Store**: Causal events mirrored to SQLite (`causal_events` table) for agent learning queries
- **Constitution Structure**: The signed constitution (`docs/constitution/versions/<date>/constitution.md`) is **principles + a Bill of Rights**. Rules (§1–§11, `P-x.y`, e.g. `P-7.19`) bind the agent; rights (§0 Bill of Rights, `Ri-x.y`) bind the gateway. The gateway is a **Lawful Executor** (deterministic enforcement, no improvised judgment); a place where it would exercise reserved judgment is a tracked **DISCRETION LEAK**. The `enforcement_register` (code) maps each numbered rule → its parent principle/right and the code+test that enforce it.
- **Contract Health**: Standing view of how often each constitutional clause (principle/right) has been enforced. Enforcement events carry their rule ID (e.g. `loop_guard.tripped` → `P-7.19`); the `enforcement_register` reverse-maps these to their owning clause, and `GatewayStore::contract_health` tallies occurrences by clause (unrecognised IDs surfaced as `unattributed`). Surfaced via `autonoetic trace contract-health`.
- **Execution Traces**: Full code execution results (stdout, stderr, exit_code) in `execution_traces` table — not truncated
- **Live Digest**: Real-time session narrative in `digest.md`, replacing flat timeline
- **Artifact Store**: Content-addressed (SHA-256) storage; agents pass handles, not inline blobs
- **RuntimeLock**: Pinned execution closure for reproducible agent runs (`runtime.lock`)
- **Cognitive Capsule**: Portable export of an agent bundle plus its runtime closure
- **Skill Promotion**: Successful tactics can be crystallized into reusable Skills
- **Turn Continuation**: Approval-gated workflow turns are suspended to disk (`.gateway/continuations/<task_id>.json`) and resumed with real tool results, avoiding synthetic retry prompts
- **Session Approval Grants**: Once the operator approves network access to specific hosts, subsequent `sandbox.exec` calls within the same root session targeting those hosts are auto-approved (stored in `session_approval_grants` SQLite table, cleaned up on session end)
- **Promotion Severity Gating**: `promotion.record` mechanically rejects `pass=true` when findings contain `error`/`critical` severity, or `warning` findings without concrete `evidence` — preventing evaluators from passing unvalidated code
- **Emergency Stop**: Root-session circuit breaker that kills processes, aborts tasks, cancels pending gates, deletes session grants
- **Retention Policy**: Configurable pruning of `execution_traces` (default: 30 days) and `causal_events` (default: 90 days)

### HTTP API

The gateway exposes a REST API for remote agents. Authentication uses HMAC. See `docs/remote-agents-http-api.md`.

### Tests

Integration tests are in `autonoetic-gateway/tests/` (30+ tests). They use `tempfile` for isolated workspaces and `serial_test` for state isolation. CLI e2e tests are in `autonoetic/tests/cli_e2e.rs`.

Notable suite for approval continuation:
- `autonoetic-gateway/tests/turn_continuation_approval_integration.rs` — suspend/resume, timeout, cancellation, restart, and parallel-join behavior

## Key Documentation

- `docs/ARCHITECTURE.md` — System design, security model, data flow
- `docs/AGENTS.md` — Agent roles, routing, capabilities, lifecycle
- `docs/CLI.md` — Complete CLI reference
- `docs/separation-of-powers.md` — Agent vs gateway responsibilities
- `docs/remote-agents-http-api.md` — HTTP API and SDK transport
- `docs/agent-learning.md` — How agents learn from past sessions using execution.search, knowledge.search_by_tags, digest.query
- `docs/planner-principles.md` — Principle-first planner design: why principles beat rules, security boundary, what moved to specialists
- `docs/agent-discovery.md` — Agent discovery: agent.list gateway tool + discovery.default semantic matching agent
