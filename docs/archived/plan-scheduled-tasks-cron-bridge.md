# Plan: Scheduled Tasks and Optional Linux Cron Bridge

**Date:** 2026-04-12  
**Status:** Draft v1  
**Related:** docs/approval-system.md, docs/workflow-orchestration.md, docs/remote-access-approval.md, docs/ARCHITECTURE.md

---

## Goal

Add first-class scheduled task support to autonoetic so agents can register recurring work safely and durably, while preserving current approval, policy, and audit guarantees.

This plan also defines an optional modular bridge for Linux crontab. The bridge is explicitly non-authoritative: gateway scheduling remains the source of truth.

---

## Non-Goals

- No replacement of existing background interval reevaluation.
- No unconstrained natural-language schedule parsing driven by LLM in v1.
- No direct execution from OS cron that bypasses gateway policy and approvals.
- No cross-platform scheduler abstraction in v1 beyond Linux bridge design.

---

## Design Principles

1. Gateway-owned scheduling remains authoritative.
2. Every scheduled trigger must be durable and auditable.
3. Scheduled execution must use existing workflow execution paths.
4. Scheduled execution must not bypass approvals, including remote-access approvals.
5. Linux cron support is an optional ingress adapter, not a second scheduler engine.

---

## Target UX Surface

### Agent-facing tools

1. scheduler.cron.create
2. scheduler.cron.list
3. scheduler.cron.pause
4. scheduler.cron.resume
5. scheduler.cron.cancel

### Minimal create input

- agent_id
- message
- schedule_expr (cron or constrained natural-language)
- timezone optional, default UTC
- metadata optional

### Minimal create output

- job_id
- normalized_cron_expr
- timezone
- next_run_at
- status

### Natural-language scheduling in v1

Accepted in v1 through deterministic pattern parsing, then canonicalized to cron:

- every 5 seconds
- every 10 minutes
- every 2 hours
- every day at 09:00
- every monday at 14:30

Rules:

- Parser is grammar-based, not model-generated.
- Output must always include normalized_cron_expr.
- Ambiguous expressions are rejected with actionable errors.
- If parsing fails, callers can submit explicit cron.

---

## Data Model and Persistence

## New persistent entity: scheduled_job

Recommended fields:

- job_id string primary key
- owner_agent_id string
- root_session_id string
- target_agent_id string
- message string
- metadata_json text nullable
- cron_expr string
- timezone string default UTC
- next_run_at string RFC3339 UTC
- last_run_at string RFC3339 UTC nullable
- status string active, paused, cancelled
- created_at string RFC3339 UTC
- updated_at string RFC3339 UTC
- last_error text nullable
- generation integer default 0 for optimistic locking

Recommended indexes:

- index status plus next_run_at
- index root_session_id
- index owner_agent_id

Implementation location:

- autonoetic-gateway/src/scheduler/gateway_store/migrate.rs
- autonoetic-gateway/src/scheduler/gateway_store directory store helpers

---

## Execution Model

1. Scheduler tick loads active jobs where next_run_at is due.
2. Each due job is claimed atomically using generation or compare-and-swap semantics.
3. Claimed jobs enqueue a workflow task for target agent plus message.
4. Scheduler computes and persists the next occurrence after successful enqueue.
5. If enqueue fails, scheduler records last_error and retries on future ticks with backoff policy.

Rationale:

- Reuses existing workflow task lifecycle and observability.
- Avoids introducing a second task runner model.

Integration points:

- autonoetic-gateway/src/scheduler.rs
- autonoetic-gateway/src/scheduler/workflow_store.rs

---

## Policy and Security

## Capability gating

Add explicit capability requirement for cron management operations in policy checks. Agents without capability cannot create or mutate jobs.

Likely touchpoints:

- autonoetic-types/src/capability.rs
- autonoetic-gateway/src/policy.rs
- agent manifests requiring scheduler capabilities

## Ownership constraints

- Agent can list or mutate only jobs it owns by default.
- Root-session scoped operator contexts may be allowed to list all jobs for that root.

## Approval invariants

Scheduled jobs trigger normal task execution. Any sandbox execution with network effects still goes through existing approval flow.

Preserved path:

- sandbox tool remote-access analysis
- approval request creation
- approved exec cache and session grants

Likely touchpoint:

- autonoetic-gateway/src/runtime/tools/sandbox.rs

---

## Phase Plan with Implementation Tasks

## Phase 1: Schedule Core Types and Parser

Tasks:

1. Add schedule domain types and validation errors.
2. Add cron parser utility with next occurrence calculation.
3. Add constrained natural-language parser that maps accepted phrases to cron.
4. Normalize both cron and natural-language input into canonical stored cron.
5. Define timezone policy, default UTC only for v1.

Deliverables:

- new scheduler cron utility module
- new constrained schedule phrase parser module
- unit tests for valid and invalid expressions

Acceptance criteria:

- invalid cron rejected with structured errors
- accepted natural-language phrases are normalized to cron deterministically
- ambiguous natural-language phrases are rejected with guidance
- next occurrence deterministic for test fixtures

## Phase 2: Durable Job Store and Migration

Tasks:

1. Add scheduled_job table migration with indexes.
2. Add gateway store methods create, list, pause, resume, cancel, claim_due, advance_next_run.
3. Add optimistic locking to avoid double-firing.

Deliverables:

- migration bump in gateway_store migration file
- store API tests

Acceptance criteria:

- due claims are single-owner under concurrent attempts
- pause or cancel prevents further triggering

## Phase 3: Scheduler Tick Wiring

Tasks:

1. Extend scheduler tick to process due scheduled jobs.
2. Convert due jobs into workflow queued tasks.
3. Persist trigger events and failures.
4. Ensure restart-safe behavior with no lost due job.

Deliverables:

- scheduler due-jobs processing path
- causal and workflow events for scheduled trigger lifecycle

Acceptance criteria:

- due job leads to exactly one queued workflow task
- restart during tick does not produce duplicate launches

## Phase 4: Agent Tooling

Tasks:

1. Implement scheduler tool module under runtime tools.
2. Register new tools in tools registry.
3. Add strict input schemas and deterministic outputs.
4. Add pagination and filtering for list operation.

Deliverables:

- scheduler tool file in runtime tools
- registry updates

Acceptance criteria:

- create and list operations work for authorized agents
- unauthorized operations fail with policy errors

## Phase 5: Policy, Approval, and Guardrails

Tasks:

1. Wire capability checks for each cron operation.
2. Add minimum interval or frequency guardrails in config and policy.
3. Verify scheduled path cannot bypass approval requirements.
4. Add per-root and per-agent job limits.

Deliverables:

- policy checks and limits
- config additions and defaults

Acceptance criteria:

- high-frequency abusive schedule is rejected
- network-sensitive scheduled execution still requests approvals

## Phase 6: Testing and Hardening

Tasks:

1. Add integration tests for cron creation to trigger to next recurrence.
2. Add approval flow integration tests for scheduled sandbox network actions.
3. Add concurrency tests for claim dedup.
4. Add pause and resume and cancel behavior tests.
5. Add emergency-stop interaction tests.

Likely tests location:

- autonoetic-gateway/tests

Acceptance criteria:

- all new tests pass consistently in serial and normal runs
- no regressions in existing scheduler and approval suites

---

## Optional Modular Linux Cron Bridge

## Purpose

Allow operators who already use Linux crontab to trigger named autonoetic jobs, without delegating scheduling authority to OS cron.

## Architectural stance

- Internal gateway scheduler remains canonical.
- Linux cron is only an external trigger mechanism.
- Trigger endpoint validates signatures, identity, and policy.

## Bridge shape

Module A, bridge exporter:

- CLI command generates crontab-safe lines that call a gateway endpoint or CLI trigger.
- Output includes stable job_id and auth token reference.

Module B, bridge receiver:

- Gateway endpoint receives trigger for a known job_id.
- Endpoint writes a trigger intent event and enqueues workflow task via same internal path as native scheduler.
- Endpoint is idempotent for repeated trigger calls in a small window.

## Security controls

- HMAC or signed token per bridge invocation.
- Optional source host allowlist for bridge calls.
- Replay window and nonce checks.

## Operational controls

- Dry-run mode for generated crontab lines.
- health and last-trigger diagnostics.
- explicit bridge enabled flag in config.

## When to use

Use Linux bridge only when:

- operations mandate OS-level scheduling tools
- jobs must be coordinated with existing host automation

Do not use Linux bridge as default in single-gateway deployments where internal scheduler suffices.

---

## Suggested Rollout

1. Milestone A

- Phase 1 and 2 complete
- hidden behind feature flag

2. Milestone B

- Phase 3 and 4 complete
- enabled in dev and staging

3. Milestone C

- Phase 5 and 6 complete
- production rollout with limits and monitoring

4. Milestone D optional

- Linux cron bridge exporter and receiver behind separate feature flag

---

## Open Decisions

1. Should timezone support in v1 stay UTC-only, or permit fixed-zone strings?
2. Should scheduled job execution always enqueue workflow tasks, or allow direct scheduled actions for selected safe primitives?
3. Should bridge endpoint live under existing API auth model, or use dedicated bridge credentials?
4. What is the default per-root scheduled job cap?
5. Which initial natural-language grammar set is mandatory in v1 beyond interval and weekday-time patterns?

---

## Implementation Checklist

1. Add schedule parser and canonicalization module.
2. Add constrained natural-language schedule parser and normalization tests.
3. Add scheduled_job migration and store methods.
4. Wire scheduler tick to claim and trigger due jobs.
5. Add scheduler.cron tool module and registration.
6. Add capability and policy checks.
7. Add guardrail config and defaults.
8. Add integration and concurrency tests.
9. Add docs updates for API and operator usage.
10. Add optional Linux bridge design and feature flag.
