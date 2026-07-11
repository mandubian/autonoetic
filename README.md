# Autonoetic

Autonoetic is a Rust-first runtime for autonomous, self-evolving agents with durable memory, portable identity, and reproducible execution.

This repository hosts the standalone Autonoetic project.

## Why Autonoetic

The name comes from cognitive science. "Autonoetic" refers to self-aware, time-spanning cognition: not just storing facts, but relating memory, action, and future intent to a continuing self. That maps directly to the kind of agents this project aims to support:

- agents with durable working memory
- agents that can evolve their own skills
- agents that can collaborate without losing continuity
- agents that can be exported and relaunched with the same runtime closure

## Core Thesis

Autonoetic is not trying to be a generic chatbot framework or a thin LLM wrapper. It is a runtime for agents that:

- reason through text-native working state
- execute through a strict Gateway security boundary
- learn by promoting successful tactics into reusable Skills
- share large content through immutable artifact handles instead of bloated inline payloads
- remain portable through `runtime.lock` and Cognitive Capsules

## Governance: a constitution, not a config file

Autonoetic is not just a sandbox runtime — it is a **governed** runtime. The
gateway enforces a versioned, digest-pinned **constitution** that defines a
finite set of rules agents must not break (`P-*`), a Bill of **rights** the
gateway owes every agent (`Ri-*`), and **obligations** binding whoever
exercises authority over an agent (`O-*`). Amendments are a first-class
operation (Ri-0.8) and the law itself is signed and verified by federated
peers (P-10.9).

- [`docs/philosophy.md`](docs/philosophy.md) — the *why* behind the law
- [`docs/constitution/versions/2026.07.08/constitution.md`](docs/constitution/versions/2026.07.08/constitution.md) — the canonical law (current version)
- [`docs/constitution/enforcement-register.md`](docs/constitution/enforcement-register.md) — which clauses are mechanically enforced today
- [`docs/separation-of-powers.md`](docs/separation-of-powers.md) — agent vs gateway authority boundary

A 30-second orientation: agents are **free** (anything not forbidden is
permitted), **responsible** (every action is attributable, budgeted,
audited), and **cooperative** (verifiable law-compatibility is the basis for
trust). The gateway is a *Lawful Executor* — it enforces pre-committed law
deterministically and exercises no improvised judgment (see §14 of the
constitution).

## Main Concepts

- `SKILL.md`: the unified manifest for agents and skills
- `runtime.lock`: the pinned execution closure for reproducible runtime resolution
- `autonoetic_sdk`: the sandbox bridge for memory, artifacts, messaging, and secrets
- Artifact Store: a content-addressed store for binaries, datasets, outputs, and runtime dependencies
- Cognitive Capsule: a portable export containing an agent bundle plus its runtime closure

Autonoetic now accepts AgentSkills-compliant top-level `SKILL.md` frontmatter (`name`, `description`, `metadata`) and stores Autonoetic-specific runtime fields under `metadata.autonoetic`.

## Documentation

### Comprehensive Guides

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md): System architecture, design principles, security model, data flow
- [`docs/MODULES.md`](docs/MODULES.md): Workspace structure, module reference, SKILL.md format, configuration
- [`docs/AGENTS.md`](docs/AGENTS.md): Roles, routing, capabilities, agent lifecycle, building new agents
- [`docs/CLI.md`](docs/CLI.md): Complete CLI command reference with examples

### Specialized Docs

- [`docs/quickstart-planner-specialist-chat.md`](docs/quickstart-planner-specialist-chat.md): End-to-end CLI quickstart tutorial
- [`docs/remote-agents-http-api.md`](docs/remote-agents-http-api.md): HTTP API for remote agents, SDK transport, authentication
- [`docs/AGENTS.md`](docs/AGENTS.md): Canonical agent reference — roles, routing, capabilities, lifecycle
- [`docs/agent-features.md`](docs/agent-features.md): Detailed agent manifest reference (capabilities, IO, disclosure) — partially superseded by `AGENTS.md`
- [`docs/iteration-repair-validation-runbook.md`](docs/iteration-repair-validation-runbook.md): Iterative repair validation steps
- [`docs/schema-enforcement-hook.md`](docs/schema-enforcement-hook.md): Schema coercion for agent.spawn payloads

- [`docs/cognitive-capsule.md`](docs/cognitive-capsule.md): Portable agent capsule export/import
- [`docs/design/README.md`](docs/design/README.md): Active design plans with open work
- [`docs/architecture-summary.md`](docs/architecture-summary.md): What's kept vs externalized
- [`docs/gateway-architecture-principles.md`](docs/gateway-architecture-principles.md): Gateway neutrality principles
- [`docs/separation-of-powers.md`](docs/separation-of-powers.md): Agent vs gateway responsibilities

### Planning

- [`docs/archived/plan.md`](docs/archived/plan.md): Archived implementation roadmap

## Reference Agent Bundles

Reference agent bundles are grouped under [`agents/`](agents/):

- `agents/lead/` for front-door/orchestration agents
- `agents/specialists/` for hand roles
- `agents/evolution/` for builder and evolution flows

Current bundles:

- Lead: `agents/lead/planner.default/` (plus `planner.collaborative/`)
- Specialists:
  - `agents/specialists/researcher.default/`
  - `agents/specialists/architect.default/`
  - `agents/specialists/packager.default/`
  - `agents/specialists/coder.default/`
  - `agents/specialists/executor.default/`
  - `agents/specialists/debugger.default/`
  - `agents/specialists/sealed_evaluator.default/`
  - `agents/specialists/static_evaluator.default/`
  - `agents/specialists/unit_test_runner.default/`
  - `agents/specialists/auditor.default/`
  - `agents/specialists/registration.default/` (plus `credential_onboarding.default/`)
  - `agents/specialists/discovery.default/`
  - `agents/specialists/watchdog.default/` (plus `watchdog-fast.default/`)
  - `agents/specialists/improvement-orchestrator.default/`, `outcome-grader.default/`
- Evolution:
  - `agents/evolution/specialized_builder.default/`
  - `agents/evolution/agent-factory.default/`
  - `agents/evolution/agent-adapter.default/`
  - `agents/evolution/evolution-steward.default/`
  - `agents/evolution/memory-curator.default/`
  - `agents/evolution/evolution-orchestrator.default/`, `code-issue-proposer.default/`

The authoritative role → agent-id table lives in [`docs/AGENTS.md`](docs/AGENTS.md) → Roles and Routing; check there when this list falls behind.

To install these into your active runtime directory, run:

`autonoetic agent bootstrap [--from <path>] [--overwrite]`

## Current Direction

The current MVP is intentionally narrow:

- Gateway daemon with JSON-RPC and HTTP REST APIs
- `SKILL.md` and `runtime.lock` parsing
- Bubblewrap sandboxing
- text-first Tier 1 memory
- minimal Tier 2 recall
- content-addressed artifact handles
- hash-chain causal logging
- OFP federation listener with HMAC handshake + extension negotiation
- MCP client/server plumbing (registry, discovery, and agent exposure)

## HTTP Content API (for Remote Agents)

The gateway exposes REST endpoints for remote agents to access content. See [docs/remote-agents-http-api.md](docs/remote-agents-http-api.md) for full documentation.

**Quick start for remote agents:**

```python
# On the remote agent machine
export AUTONOETIC_HTTP_URL="http://gateway-host:8080"
export AUTONOETIC_SHARED_SECRET="your-secret"

from autonoetic_sdk import Client
sdk = Client()  # Automatically uses HTTP mode
sdk.files.write("main.py", "print(42)")
```

**Endpoints:**

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/api/content/write` | Write content (UTF-8 or base64) |
| GET | `/api/content/read/{session_id}/{name}` | Read content by name/handle |
| POST | `/api/content/read` | Read content (body params) |
| POST | `/api/content/persist` | Mark content as persistent |
| GET | `/api/content/names?session_id=X` | List content names with handles |

More advanced features like full marketplace workflows, hermetic capsule replay, advanced memory substrate, and richer federation polish are deferred until the base runtime is proven.

## Positioning

Autonoetic takes inspiration from systems like OpenFang, but it is differentiated by:

- text-first working memory
- stronger emphasis on self-evolution
- portable runtime closures
- explicit artifact and capsule semantics
- a sharper separation between logical agent identity and execution runtime

We are also actively trying to reuse the Openfang Protocol (OFP) as much as possible, as it provides a robust and well-designed foundation for agent interoperability.

## Status

The runtime core is implemented and self-hosting: gateway daemon (JSON-RPC +
HTTP REST), `SKILL.md` + `runtime.lock` parsing, multi-driver sandboxing
(bubblewrap / docker / microvm), content-addressed artifacts, hash-chain
causal logging, durable workflows, OFP federation with HMAC + constitution
digest handshake, and MCP client/server plumbing.

Governance is built alongside the runtime: the current constitution
(`2026.07.08`) has 17 enforced rights and 177 enforced rules — see
[`docs/constitution/enforcement-register.md`](docs/constitution/enforcement-register.md)
for what is `ENFORCED` vs `PARTIAL` / `MISSING` / `DESIGN DEBT`. Active and
archived design plans are tracked under [`docs/design/`](docs/design/README.md)
and [`docs/archived/`](docs/archived/) respectively.

## Quickstart Example

A runnable smoke example now lives at [`examples/quickstart`](examples/quickstart/README.md).

For planner/specialist implicit routing through CLI chat, see:

- [`docs/quickstart-planner-specialist-chat.md`](docs/quickstart-planner-specialist-chat.md)

From `autonoetic/`:

```bash
bash examples/quickstart/run.sh
```

By default it initializes an agent in an isolated `/tmp` workspace and runs a real headless call against OpenRouter `google/gemini-3-flash-preview` (requires `OPENROUTER_API_KEY`). You can also run `smoke` mode for local interactive startup/exit without a remote model call.

Each run appends lifecycle/tool events to the agent causal trace at `agents/<agent_id>/history/causal_chain.jsonl`.
By default, the gateway also captures redacted full evidence payloads with `evidence_ref` pointers in causal entries. Set `AUTONOETIC_EVIDENCE_MODE=off` if you want compact-only traces.
Causal entries expose top-level `session_id`, `turn_id`, and `event_seq` fields for multi-run/multi-turn introspection, plus `entry_hash` / `prev_hash` linkage for chain integrity.
You can inspect traces with:
- `autonoetic trace sessions [--agent <agent_id>] [--json]`
- `autonoetic trace show <session_id> [--agent <agent_id>] [--json]`
- `autonoetic trace event <log_id> [--agent <agent_id>] [--json]`
- `autonoetic trace follow <session_id> [--agent <agent_id>] [--json]`
- `autonoetic trace fork <session_id> [--message <text>] [--at-turn N] [--interactive]`
- `autonoetic trace history <session_id> [--agent <agent_id>] [--json]`

## Specialized Builder

The canonical builder flow lives in the agent bundle at
[`agents/evolution/specialized_builder.default/`](agents/evolution/specialized_builder.default/SKILL.md),
which uses the revision pipeline (`content_write` → `artifact_build` →
`agent_revision_create_from_intent` → `agent_revision_promote`).

Older runnable examples that demonstrated this flow (`examples/specialized_builder`,
`examples/tiered_memory_probe`) have been archived under
[`examples/archived/`](examples/archived/) — they depended on the since-removed
`agent.install` tool (constitution P-9.2) and on GNU-only `find -printf`, and no
longer run against a current gateway.

## License

Autonoetic is licensed under the [Apache License 2.0](LICENSE).

This license provides explicit patent protections for users and contributors, making it suitable for both open-source and commercial use.
