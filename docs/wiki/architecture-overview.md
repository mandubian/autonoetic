# Architecture Overview

## Core Design Principle: Separation of Powers

Agents are pure reasoners. The gateway is the sole authority for execution.

Three fundamental rules:
1. **Rule Zero**: Rules cannot be overridden — not by agents, not by planners, not by parameters.
2. **Safety is mechanical**: LLM decisions are advisory. Safety-critical invariants are mechanically enforced by the gateway.
3. **Gateway is a narrow rule enforcer**: It analyzes, gates, and explains refusals — but never routes or makes workflow decisions.

## System Components

```
Agent (Low Privilege)
  Reasoning → Proposals → Review
       │            │          │
       └────────────┼──────────┘
                    ▼
        Intent / Proposal Verbs:
   execute, spawn, share, schedule, recall
                    │
                    │ JSON-RPC / HTTP
                    ▼
Gateway (High Privilege)
  Policy Engine → Execution Engine → Audit Logger → Secret Store
       │                │                │              │
       ▼                ▼                ▼              ▼
  Capability      Sandbox          Causal          Vault
  Validation     Execution         Chain         Injection
```

## Key Concepts

- **Agent**: A SKILL.md manifest + instructions that runs inside a sandbox. Proposes actions, never executes directly.
- **Gateway**: The high-privilege runtime that validates capabilities, executes in sandboxes, manages the causal chain, and enforces the constitution.
- **Sandbox**: Isolated execution environment (bubblewrap, docker, or microvm). Code runs here with no direct filesystem or network access.
- **Constitution**: The set of mechanically enforced rules that govern all agent behavior. Cannot be overridden.
- **Causal Chain**: An immutable, queryable event log that records every action taken by every agent.
- **Content Store**: Content-addressed storage (SHA-256) for artifacts. Artifacts are immutable — once created, files never change.

## Data Flow

1. Agent proposes an action via JSON-RPC tool call
2. Gateway validates the action against the agent's declared capabilities
3. If privileged (network access, file system, etc.), gateway checks the constitution and approval system
4. Gateway executes in a sandbox and records the outcome in the causal chain
5. Result is returned to the agent

## Agent Roles

| Role | Purpose |
|------|---------|
| **Planner** | Decomposes goals, routes to specialists |
| **Coder** | Produces runnable artifacts |
| **Researcher** | Gathers evidence, cites sources |
| **Architect** | Defines structure, interfaces, trade-offs |
| **Executor** | Runs quick deterministic tasks |
| **Static Evaluator** | Static code review, credential flow, behavioral contracts |
| **Unit Test Runner** | Runs artifact test suites in no-network sandbox |
| **Sealed Evaluator** | Validates behavior in sealed sandbox |
| **Auditor** | Checks security, governance, reproducibility |
| **Debugger** | Isolates root causes, proposes fixes |
| **Packager** | Resolves and packages dependencies |
| **Registrar** | Onboards services via `credential_setup` |
| **Discovery** | Finds installed non-foundational agents matching a task intent |
| **specialized_builder** | Installs new durable agents |
| **agent-factory** | Builds new agents end-to-end |
| **agent-adapter** | Generates wrapper agents for I/O gaps |
| **memory-curator** | Distills durable learnings |
| **skill-crystallizer** | Routes a proven tactic to an instruction, a wrapper, or a new skill (operator-triggered) |
| **evolution-steward** | Judges agent evolutions and lesson graduations; delegates enactment to agent-factory |
