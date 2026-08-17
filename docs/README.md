# Autonoetic Documentation Index

Stable reference material lives at the top level of `docs/`. Active plans with
open work are in [`design/`](design/README.md). Completed or superseded plans
are in [`archived/`](archived/).

## Core Architecture

- [`autonoetic-concepts-for-beginners.md`](./autonoetic-concepts-for-beginners.md) — Beginner-friendly conceptual guide: constitution, agent rights/rules, gateway authority, capabilities, artifacts, and how Autonoetic differs from direct-agent systems.
- [`ARCHITECTURE.md`](./ARCHITECTURE.md) — Full system architecture and runtime model.
- [`architecture-summary.md`](./architecture-summary.md) — Short architecture overview.
- [`gateway-architecture-principles.md`](./gateway-architecture-principles.md) — Gateway design principles.
- [`MODULES.md`](./MODULES.md) — Module-level map of the codebase.
- [`AGENTS.md`](./AGENTS.md) — **Canonical** agent reference: roles, routing, SKILL.md format, capabilities, lifecycle.
- [`separation-of-powers.md`](./separation-of-powers.md) — Agent vs gateway authority boundary.
- [`agent-features.md`](./agent-features.md) — *Partially superseded by `AGENTS.md`*; still has unique detail on middleware / disclosure / background scheduling.
- [`agent_routing_and_roles.md`](./archived/agent_routing_and_roles.md) — *Archived / deprecated*; describes a removed default-front-door routing model. Use `AGENTS.md` → Routing Rules instead.
- [`agent-capabilities.md`](./archived/agent-capabilities.md) — *Archived / superseded by `AGENTS.md` → Capabilities System* (kept in sync with `capability.rs`).
- [`agent-adapter-specialist.md`](./agent-adapter-specialist.md) — Adapter specialist contract.

## Workflow and Interaction

- [`workflow-orchestration.md`](./workflow-orchestration.md) — Durable workflow/task lifecycle (live reference for mechanical orchestration).
- [`human-agent-collaboration.md`](./human-agent-collaboration.md) — PlanFrame, workbench projection, reconciliation, semantic summaries, validation waivers, and the `/return` handoff.
- [`archived/human-gate-unification-plan.md`](./archived/human-gate-unification-plan.md) — GateService migration (archived; shipped — residual tool migrations + agent-as-decider deferred).
- [`design/constitution-gate-amendments.md`](./design/constitution-gate-amendments.md) — Unified gate constitutional rationale.
- [`approval-notification-delivery.md`](./approval-notification-delivery.md) — Approval delivery paths.
- [`agent-clarification-protocol.md`](./agent-clarification-protocol.md) — Clarification signal format.
- [`quickstart-planner-specialist-chat.md`](./quickstart-planner-specialist-chat.md) — Planner/specialist walkthrough.

## Runtime, Storage, and Budgets

- [`gateway-store-schema.md`](./gateway-store-schema.md) — **SQLite schema reference**: every table, column, owner module, relation, and usage status (audited against `migrate.rs`).
- [`content-store.md`](./content-store.md) — Content addressing and visibility.
- [`cognitive-capsule.md`](./cognitive-capsule.md) — Portable agent capsule export/import (implemented).
- [`agent-learning.md`](./agent-learning.md) — Learning and memory retrieval.
- [`context-compression.md`](./context-compression.md) — Context governor and overflow handling.
- [`prompt-budget.md`](./prompt-budget.md) — Prompt budget controls.
- [`prompt-burden-study.md`](./prompt-burden-study.md) — What makes the system prompt large and what actually shrank it: per-layer measurement, levers that worked vs. did not, and the rules to apply before adding doctrine.
- [`agent-prompt-guidance.md`](./agent-prompt-guidance.md) — How the prompt is composed, and how to add doctrine (foundation layers, guidance blocks, phase and section gates, output contract).
- [`session-budget.md`](./session-budget.md) — Session budget behavior.
- [`budget-management.md`](./budget-management.md) — Broader budget policies.
- [`approved-resources-caching.md`](./approved-resources-caching.md) — Approval/exec cache.
- [`response-validation-gate.md`](./response-validation-gate.md) — Response contract validation.

## Security, Analysis, and Governance

- [`remote-access-approval.md`](./remote-access-approval.md) — Remote access detection and gating.
- [`credential-management.md`](./credential-management.md) — Credential vault (live reference for multi-credential).
- [`code-analysis.md`](./code-analysis.md) — Static analysis model.
- [`schema-enforcement-hook.md`](./schema-enforcement-hook.md) — Schema enforcement.
- [`agent-capabilities.md`](./archived/agent-capabilities.md) — *Archived / superseded*; use `AGENTS.md` → Capabilities System.
- [`security-sentinel.md`](./security-sentinel.md) — Divergence sentinel overview.
- [`gateway-constitution-roadmap.md`](./gateway-constitution-roadmap.md) — Constitutional gap-closure backlog.
- [`constitution-signing.md`](./constitution-signing.md) — Constitution lock and signing.
- [`iteration-repair-validation-runbook.md`](./iteration-repair-validation-runbook.md) — Repair/validation runbook.
- [`civic-eval-measurement-runbook.md`](./civic-eval-measurement-runbook.md) — Measurement procedure for the E.3 binding flip and C.2 strict-readiness decisions.

## CLI and External Interfaces

- [`CLI.md`](./CLI.md) — Main CLI reference.
- [`cli-reference.md`](./cli-reference.md) — Command reference.
- [`config-reference.md`](./config-reference.md) — Gateway configuration.
- [`remote-agents-http-api.md`](./remote-agents-http-api.md) — HTTP API for remote agents.

## Specs and Comparisons

- [`spec-build-layers-dependency-resolution.md`](./spec-build-layers-dependency-resolution.md)
- [`spec-capability-driven-sandbox-isolation.md`](./spec-capability-driven-sandbox-isolation.md)
- [`spec-implicit-artifacts-agent-evolution.md`](./spec-implicit-artifacts-agent-evolution.md)
- [`comparison-hermes-agent.md`](./comparison-hermes-agent.md)

## Active Design (`design/`)

See [`design/README.md`](design/README.md) for the full, status-annotated
table — that file is the source of truth and this index intentionally does
not duplicate it (highlights here go stale). As of the `2026.07.08`
constitution, items that were recently active and are now largely shipped
include:

- **Constitution restructure** (P-x.y format + enforcement register) — see [`constitution/enforcement-register.md`](./constitution/enforcement-register.md)
- **Gate unification** — `GateService` is the single pipeline for all `GateKind`s (constitution §2, P-2.18 `ENFORCED`)
- **Agent-as-decider** — the `GateDecider` capability is `ENFORCED` (P-2.20); the *broader* multi-decider / voting-weight vision remains a draft RFC ([`design/principal-model-and-symmetric-obligations.md`](./design/principal-model-and-symmetric-obligations.md))

Items with genuinely open work (divergence-sentinel P4 validation,
self-improvement loop P5–P7, operator approval inspection Phase 2,
post-promotion review Tier 2, and several draft RFCs) are tracked in the
design table.

## Archived (`archived/`)

Historical plans, reviews, and superseded architecture notes. Not
source-of-truth. Notable completed work now archived from `design/`:

- Promotion federation (plan + reviews) — see [`archived/approval-system-hardening-plan.md`](./archived/approval-system-hardening-plan.md)
- Sealed-network evaluation, recording mode, sealed evaluator replay
- Progressive UX / auto-learning, context overflow mitigation
- Cognitive capsule implementation plan (reference: [`cognitive-capsule.md`](./cognitive-capsule.md))
- Early architecture (`concepts.md`, `architecture_modules.md`, `protocols.md`, …)
- Pre-unification [`approval-system.md`](./archived/approval-system.md) and mechanical orchestration plan/RFC
