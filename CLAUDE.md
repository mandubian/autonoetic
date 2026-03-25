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
- **Execution Traces**: Full code execution results (stdout, stderr, exit_code) in `execution_traces` table — not truncated
- **Live Digest**: Real-time session narrative in `digest.md`, replacing flat timeline
- **Artifact Store**: Content-addressed (SHA-256) storage; agents pass handles, not inline blobs
- **RuntimeLock**: Pinned execution closure for reproducible agent runs (`runtime.lock`)
- **Cognitive Capsule**: Portable export of an agent bundle plus its runtime closure
- **Skill Promotion**: Successful tactics can be crystallized into reusable Skills
- **Turn Continuation**: Approval-gated workflow turns are suspended to disk (`.gateway/continuations/<task_id>.json`) and resumed with real tool results, avoiding synthetic retry prompts
- **Emergency Stop**: Root-session circuit breaker that kills processes, aborts tasks, cancels pending gates
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
