# Autonoetic Documentation Index

This index links to all documentation under `docs/` and marks what is stable reference material versus design/draft content.

## Core Architecture

- [`ARCHITECTURE.md`](./ARCHITECTURE.md) - Full system architecture and runtime model.
- [`architecture-summary.md`](./architecture-summary.md) - Short architecture overview.
- [`gateway-architecture-principles.md`](./gateway-architecture-principles.md) - Gateway design principles and constraints.
- [`MODULES.md`](./MODULES.md) - Module-level map of the codebase.
- [`AGENTS.md`](./AGENTS.md) - Agent model, SKILL format, lifecycle, and capabilities.
- [`separation-of-powers.md`](./separation-of-powers.md) - Authority boundary between agents and gateway.
- [`agent-features.md`](./agent-features.md) - Agent capability overview and platform features.
- [`agent_routing_and_roles.md`](./agent_routing_and_roles.md) - Routing rules and role definitions.
- [`agent-adapter-specialist.md`](./agent-adapter-specialist.md) - Adapter specialist behavior and contract.

## Workflow and Interaction Semantics

- [`design/human-gate-unification-plan.md`](./design/human-gate-unification-plan.md) - **Unified GateService architecture** for approvals, `user_ask`, and escalations (`runtime/human_gate.rs`).
- [`design/constitution-gate-amendments.md`](./design/constitution-gate-amendments.md) - Active constitutional rationale for unified gates; ratified rules are mixed with still-pending agent-decider amendments.
- [`design/gateway-mechanical-orchestration-implementation-rfc.md`](./design/gateway-mechanical-orchestration-implementation-rfc.md) - Implemented mechanical orchestration RFC: typed failures, parent wake-up, single-flight dedupe, and stage-local retry.
- [`workflow-orchestration.md`](./workflow-orchestration.md) - Durable workflow/task lifecycle and join semantics.
- [`approval-notification-delivery.md`](./approval-notification-delivery.md) - Delivery path for workflow vs non-workflow approvals.
- [`agent-clarification-protocol.md`](./agent-clarification-protocol.md) - Clarification signal format and parent/child handling.
- [`quickstart-planner-specialist-chat.md`](./quickstart-planner-specialist-chat.md) - End-to-end planner/specialist chat walkthrough.

## Runtime, Storage, and Budgets

- [`content-store.md`](./content-store.md) - Content addressing and visibility model.
- [`agent-learning.md`](./agent-learning.md) - Learning and memory retrieval patterns.
- [`session-budget.md`](./session-budget.md) - Session budget controls and behavior.
- [`budget-management.md`](./budget-management.md) - Broader budget policies and controls.
- [`approved-resources-caching.md`](./approved-resources-caching.md) - Caching strategy for approved resources.
- [`response-validation-gate.md`](./response-validation-gate.md) - Response contract validation and repair loop.

## Security, Analysis, and Governance

- [`remote-access-approval.md`](./remote-access-approval.md) - Remote access detection and approval gating.
- [`code-analysis.md`](./code-analysis.md) - Static analysis model and providers.
- [`schema-enforcement-hook.md`](./schema-enforcement-hook.md) - Schema enforcement behavior for structured payloads.
- [`agent-capabilities.md`](./agent-capabilities.md) - Capability catalog and policy implications.
- [`iteration-repair-validation-runbook.md`](./iteration-repair-validation-runbook.md) - Operational runbook for repair/validation loops.

## CLI and External Interfaces

- [`CLI.md`](./CLI.md) - Main CLI reference.
- [`cli-reference.md`](./cli-reference.md) - Command reference and examples.
- [`remote-agents-http-api.md`](./remote-agents-http-api.md) - HTTP API for remote agents and transports.

## Design Notes and Draft Specs

- [`plan-build-layers-dependency-resolution.md`](./plan-build-layers-dependency-resolution.md) - Build-layer dependency plan.
- [`spec-build-layers-dependency-resolution.md`](./spec-build-layers-dependency-resolution.md) - Build-layer dependency spec.
- [`plan-capability-driven-sandbox-isolation.md`](./plan-capability-driven-sandbox-isolation.md) - Capability-driven sandbox plan.
- [`spec-capability-driven-sandbox-isolation.md`](./spec-capability-driven-sandbox-isolation.md) - Capability-driven sandbox spec.
- [`spec-implicit-artifacts-agent-evolution.md`](./spec-implicit-artifacts-agent-evolution.md) - Implicit artifact and evolution design (Part 1 implemented; Part 2 not yet).
- [`comparison-hermes-agent.md`](./comparison-hermes-agent.md) - Comparative analysis against Hermes-Agent: feature gaps, design proposals.
- [`plan-hermes-gap-closure.md`](./plan-hermes-gap-closure.md) - Implementation plan for closing capability gaps identified in the Hermes comparison (7 independent features).

## Completed Plans (Archived)

- [`plan-agent-revision-evaluation-federation-mvp.md`](./plan-agent-revision-evaluation-federation-mvp.md) - ✅ DONE — Immutable revision model, alias-based activation, eval suite, federation provenance.
- [`plan-tools-modularization.md`](./plan-tools-modularization.md) - ✅ DONE — Split monolithic tools.rs (8,863 lines) into 14 topic-focused modules.

## Design Subdirectory

- [`design/progressive-ux-auto-learning-plan.md`](./design/progressive-ux-auto-learning-plan.md) - Progressive UX and default self-improvement: one-command start, auto-learning, contextual "why", complexity profiles, session continuity, user persona.
- [`design/human-gate-unification-plan.md`](./design/human-gate-unification-plan.md) - Unified GateService architecture, migration status, and future agent-as-decider design.
- [`design/constitution-gate-amendments.md`](./design/constitution-gate-amendments.md) - Active constitutional amendments rationale for unified gates.
- [`design/gateway-mechanical-orchestration-implementation-rfc.md`](./design/gateway-mechanical-orchestration-implementation-rfc.md) - Implemented RFC for gateway-owned workflow mechanics.
- [`design/architecture_modules.md`](./design/architecture_modules.md) - Architecture decomposition by module.
- [`design/concepts.md`](./design/concepts.md) - Core conceptual model and terminology.
- [`design/data_models.md`](./design/data_models.md) - Data model design details.
- [`design/protocols.md`](./design/protocols.md) - Protocol contracts and interactions.
- [`design/cli_interface.md`](./design/cli_interface.md) - CLI interface design.
- [`design/sandbox_sdk.md`](./design/sandbox_sdk.md) - Sandbox SDK design notes.
- [`design/promotion-federation-plan.md`](./design/promotion-federation-plan.md) - Promotion federation: multi-role evaluation jury, operator escalation, FullJury gate.
- [`design/promotion-federation-plan-review.md`](./design/promotion-federation-plan-review.md) - Independent review of Phase 1 federation implementation.
- [`design/promotion-federation-followup-review.md`](./design/promotion-federation-followup-review.md) - Follow-up review covering Phases 2-4 federation work.
- [`design/post-promotion-review-design.md`](./design/post-promotion-review-design.md) - Phase 4 post-promotion background review (observability + drift detection).
- [`design/recording-mode-design.md`](./design/recording-mode-design.md) - Phase 2 recording mode for fixture capture.
- [`design/sealed-evaluator-replay-design.md`](./design/sealed-evaluator-replay-design.md) - Phase 3 sealed evaluator replay from recorded fixtures.
- [`design/operator-approval-inspection-plan.md`](./design/operator-approval-inspection-plan.md) - Code excerpts and risk summaries in approval cards.

## Archived Documents

These are preserved for history and should not be treated as current source-of-truth:

- [`archived/agent-install-approval-retry.md`](./archived/agent-install-approval-retry.md) - Archived install approval/retry model.
- [`archived/spec-artifact-dedup-approval-improvements.md`](./archived/spec-artifact-dedup-approval-improvements.md) - Archived mixed draft spec.
- [`archived/plan_workflow_update.md`](./archived/plan_workflow_update.md) - Archived workflow update plan.
- [`archived/plan_signal.md`](./archived/plan_signal.md) - Archived signal delivery plan.
- [`archived/plan_approval_response_details.md`](./archived/plan_approval_response_details.md) - Archived approval response details.
- [`archived/plan_adapt.md`](./archived/plan_adapt.md) - Archived adaptation planning notes.
- [`archived/promotion-strategy.md`](./archived/promotion-strategy.md) - Archived promotion strategy.
- [`archived/spec-agent-revision-evaluation-federation-mvp.md`](./archived/spec-agent-revision-evaluation-federation-mvp.md) - Design spec for the completed revision/eval/federation MVP.
- [`archived/tool-skill-repository-design.md`](./archived/tool-skill-repository-design.md) - Aspirational tool/skill repository design (not yet implemented).
- [`archived/approval-system.md`](./archived/approval-system.md) - Pre-unification approval architecture (superseded by `GateService`).
- [`archived/architecture-interaction-mechanisms.md`](./archived/architecture-interaction-mechanisms.md) - Legacy three-pipeline interaction model (superseded by unified gate).
- [`archived/gateway-mechanical-orchestration-plan.md`](./archived/gateway-mechanical-orchestration-plan.md) - Superseded pre-implementation design plan for the mechanical orchestration RFC.
