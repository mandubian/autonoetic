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

**Always use `cargo nextest run`, never `cargo test`, for verification.**
`cargo test` executes the gateway's 38 integration-test binaries sequentially and
`#[serial]` tests serialize within each binary, so its wall time collapses to
the sum of serial test time (~10 min full workspace measured on a 32-core dev
box) while `cargo nextest run` runs tests as processes across all cores
(~1 min). If nextest is missing, install it first — do not fall back to
`cargo test`:

```bash
curl -LsSf https://get.nexte.st/latest/linux | tar zxf - -C ~/.cargo/bin
# or: cargo install cargo-nextest --locked
```

```bash
cargo nextest run                              # Run all tests
cargo nextest run -p autonoetic-gateway        # Gateway crate only (most tests live here)
cargo nextest run -E 'test(turn_continuation)' # Run tests matching a name filter
cargo nextest run --profile ci                 # Serial CI semantics (flake reproduction)
RUST_LOG=autonoetic=debug cargo nextest run    # Run with debug logging
cargo test --doc                               # Doc-tests only (nextest does not run these)
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
python3 docs/constitution/recompute_lock.py --version 2026.09.02 \
  --signing-sk-b64 "$AUTONOETIC_CONSTITUTION_SIGNING_SK_B64"
```

To intentionally rotate signer material:

```bash
python3 docs/constitution/recompute_lock.py --version 2026.09.02 --generate-key
```

If signer key rotated, update `trusted_signers` for
`autonoetic:constitution:v1` in:

- `autonoetic-types/src/config.rs`
- `config/config-template.yaml`
- `docs/reference/config.md`

Then validate (the lock test is a `--lib` unit test; the constitution suites
live in the `constitution` domain binary — there is no
`constitution_r_8_6_*` binary anymore):

```bash
cargo nextest run -p autonoetic-gateway --lib constitution_lock_matches_canonical_digest_and_counts
cargo nextest run -p autonoetic-gateway --test constitution r_8_6_retention_policy_startup
```

Canonicalization details are documented in `docs/constitution/signing.md`.
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
- **Lesson Graduation**: A lesson that recurs across sessions is graduated into an agent's SKILL.md instruction text through the curator → steward → `agent-factory` pipeline, landing as an audited revision behind the normal promotion gates
- **Skill Crystallization**: `/crystallize` in the session room fires `skill-crystallizer.default` on the session an operator is watching. It reads the evidence and returns a routing verdict — `graduate` (instruction on an existing agent), `adapt` (wrapper via `agent-adapter.default`), `crystallize` (new reusable skill via `agent-factory.default`), or `none` — records it in `evolution/crystallizations`, and delegates enactment. It never installs: every route ends at a Candidate revision behind the promotion gates, and a new agent identity also needs operator approval. Operator-triggered only; the autonomous curator-proposed variant is #880. `/skills` (RPC `evolution.list_pending`, assembled by `evolution_view.rs`) is the standing view of proposals, recorded decisions, and Candidate revisions awaiting promotion
- **Turn Continuation**: Approval-gated workflow turns are suspended to disk (`.gateway/continuations/<task_id>.json`) and resumed with real tool results, avoiding synthetic retry prompts
- **Session Approval Grants**: Once the operator approves network access to specific hosts, subsequent `sandbox.exec` calls within the same root session targeting those hosts are auto-approved (stored in `session_approval_grants` SQLite table, cleaned up on session end)
- **Promotion Severity Gating**: `promotion.record` mechanically rejects `pass=true` when findings contain `error`/`critical` severity, or `warning` findings without concrete `evidence` — preventing evaluators from passing unvalidated code
- **Emergency Stop**: Root-session circuit breaker that kills processes, aborts tasks, cancels pending gates, deletes session grants
- **Retention Policy**: Configurable pruning of `execution_traces` (default: 30 days), `causal_events` (default: 90 days) and `post_promotion_reviews` (default: 90 days)

### HTTP API

The gateway exposes a REST API for remote agents. Authentication uses HMAC. See `docs/reference/http-api.md`.

### Tests

Integration tests are in `autonoetic-gateway/tests/` (38 domain binaries,
`tests/<domain>/main.rs` with one module per former file — no top-level
`tests/*.rs` files remain; suites binding ports or fixed paths stay in their
own binary). They use `tempfile` for isolated workspaces and `serial_test` for
state isolation. CLI e2e tests are in `autonoetic/tests/cli_e2e.rs`.

Notable suite for approval continuation:
- `autonoetic-gateway/tests/turn_continuation_approval_integration.rs` — suspend/resume, timeout, cancellation, restart, and parallel-join behavior

**Docs are mechanically guarded.** Editing anything under `docs/` can fail a
test, so run these before assuming a doc change is free:

```bash
cargo nextest run -p autonoetic-gateway --lib docs_link_guard   # paths, relative links, anchors, labels, symbols, proposals index
cargo nextest run -p autonoetic-gateway --lib wiki              # wiki canonical pointers + digest budget + config/env citations
cargo nextest run -p autonoetic --bins docs_coverage            # every CLI subcommand documented; documented globals exist
```

What they enforce: a cited `docs/…` path or relative link resolves; a `#anchor`
matches a real heading; a link label that names a file names *the* file linked; a
backticked type name exists in Rust or SDK sources; every proposal is listed in
`docs/proposals/README.md`; **every clause ID printed on a `.svg`/`.html` under
`docs/`, or in any scanned Markdown, is declared by the active constitution**;
every wiki page names a `canonical` doc and stays under 200 lines; every
`autonoetic` subcommand appears in `docs/reference/cli.md`.

The two clause checks differ on purpose. On a diagram a bare `P-3` **fails**: a
badge reading `P-3` is read as a clause citation, so write `§3` for a section or
`P-*` for a family. In prose it **resolves against the declared sections** — the
enforcement register genuinely groups by section and names the group — while a
family ID naming no real section still fails.

Intentional exceptions go in `docs/.link-guard-allow` / `docs/.symbol-guard-allow`
/ `docs/.clause-guard-allow` with a reason each — prefer rewording over an entry.
A clause entry is for a document whose *subject* is a clause not in force (a
reserved `O-4`, a fabricated `U-4` shown as a counterexample), and it is expected
to expire. `docs/archived/**` and
`docs/constitution/versions/**` are deliberately unscanned (historical records;
digest-signed bytes). These live in the **lib/bin** targets because PR CI runs
`--lib --bins` only.

**Ignored (manual) sandbox e2e** — require a host `bwrap` and execute real code, so they are NOT run in CI:
- `autonoetic-gateway/tests/promotion/gate_mocked_network_e2e.rs` (module in the `promotion` domain binary) — proves the promotion gate runs a `urllib`-importing but mocked test suite offline (passes) and that a real network call fails offline. Run with:
  ```bash
  cargo nextest run -p autonoetic-gateway --test promotion gate_mocked_network_e2e \
    --run-ignored ignored-only --nocapture
  ```
  Its CI-safe decision counterpart (no sandbox) is `promotion/gate_network_isolation_decision.rs`.

## Key Documentation

- `docs/ARCHITECTURE.md` — System design, security model, data flow
- `docs/concepts/philosophy.md` — The conceptions behind the constitution: functional autonoesis, bind-direction social contract, correctability over perfection, democratic trajectory, end-user primacy, and the intellectual lineage (Tulving, Fuller, Hart, Popper, Ostrom, Hirschman, Rawls…)
- `docs/AGENTS.md` — Agent roles, routing, capabilities, lifecycle
- `docs/reference/cli.md` — Complete CLI reference
- `docs/concepts/separation-of-powers.md` — Agent vs gateway responsibilities
- `docs/reference/http-api.md` — HTTP API and SDK transport
- `docs/guide/agent-learning.md` — How agents learn from past sessions using execution_search, knowledge_search, digest_query
- `docs/concepts/planner-principles.md` — Principle-first planner design: why principles beat rules, security boundary, what moved to specialists
- `docs/reference/agent-discovery.md` — Agent discovery: agent.list gateway tool + discovery.default semantic matching agent
- `docs/internals/prompt/composition.md` — How the system prompt is composed: foundation layers, the guidance-block mechanism (tool/capability/model/role/**phase**-gated, `NativeTool::guidance()`), **`sections:` gates for `SKILL.md` role doctrine**, the `io.returns` Output Contract, the SKILL.md doctrine regression guard, and the three tests to apply before adding doctrine
- `docs/internals/prompt/burden-study.md` — Why the prompts are the size they are and what actually shrank them: the per-layer measurement, the levers that worked (**ownership beat compression ~7:1**) and the ones that did not (moving prose into a `ToolPresent` block is a token wash), the silent-drift failure mode behind every defect found, and the enforced per-agent budget. Read before adding prompt weight
- `docs/guide/session-forking.md` — Forking a session from a past turn: runnable Hibernation checkpoint, yield-point granularity, timeline mirroring (copy vs reuse-by-reference choice), CLI/RPC/room surfaces
- `docs/internals/sandbox/sink-detection.md` — Why network detection resolves **sinks** (the closed stdlib/builtin primitive set) through the code's own import bindings instead of matching library names (#1021): what the library-name treadmill missed (including stdlib `http.client`), the Python/JS/Go/Rust coverage analysis, why a Rust resolver beats a `python3` AST subprocess, and the detection-vs-declaration boundary held for #1023
- `docs/internals/sandbox/network-grant.md` — Why `sandbox_exec` network reachability is a per-exec **grant**, not a capability-inherited **ceiling** (#1022 audit): the pre-fix bypass chain, why zero-signal egress is reachable without obfuscation, the decision table in `runtime/network_grant.rs`, and which sibling exec paths are capability-driven by design
- `docs/internals/sandbox/drivers.md` — The `SandboxDriver` trait + registry (#1117): what each backend owns (command construction, SDK bridge plumbing, child env, network guarantee, dependency support), the process vs in-process tier split, why `guarantees_network_off` is per-driver and fails closed, and the three edits it takes to add a backend
- `docs/internals/sandbox/wasm-tier.md` — Portable in-process WASM tier (`sandbox: "wasm"`) and first-class JavaScript agents (compiled to wasm via Javy at bootstrap): concepts, the `wasm-tier` build feature, `gateway preflight`, resource bounds, and a JS-agent tutorial. python.wasm is deferred (see the RFC status note).
- `docs/internals/storage/content-visibility.md` — How content written in one session becomes readable in another: the three `ContentVisibility` levels, why reachability is decided by *where a write propagated* rather than by searching peer sessions (which is what makes `Private` a guarantee), the root-not-parent propagation target and the absent "caller only" level, why a failed `set_root_session` makes `Session` silently behave like `Private`, and why refs beat names across sessions
