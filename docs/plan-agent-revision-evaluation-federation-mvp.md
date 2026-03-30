# Implementation Plan: Agent Revision, Evaluation, and Federation MVP

**Spec:** [spec-agent-revision-evaluation-federation-mvp.md](spec-agent-revision-evaluation-federation-mvp.md)
**Date:** 2026-03-30

This plan is execution-oriented.

It assumes:

- no backward-compatibility track;
- no runtime fallback to mutable authoring directories;
- no `agent.install` path;
- one mutable alias per logical agent in MVP;
- explicit `agent_ref` can target candidate revisions without an alias.

## Progress Snapshot

Landed work so far covers a subset of Phase 0, most of the Phase 1 registry and resolver scaffolding, and the core Phase 2 promote/rollback commands.

- Phase 0 simplification landed for explicit ingress targeting, config cleanup, role-gate removal, and binary disclosure classes.
- Phase 1a landed for revision and eval Rust types, `SessionAgentBinding` shape, `LockedLayerMount`, and `ArtifactKind::AgentBundle`.
- Phase 1b-1e landed for revision, alias, session-binding, promotion, and eval tables; alias-backed resolver behavior; short revision refs; revision create/list/inspect; and first-pass session pinning on spawn.
- Phase 2 landed for transactional promote/rollback commands with same-agent validation, default rollback-to-previous behavior, durable promotion history, and optional eval-run gating.
- `agent.install` still exists in the current codebase as a transitional path, so the plan assumptions remain the target architecture rather than the fully completed current state.

## Global Invariants

These invariants must hold after every phase:

- [x] Runtime execution never resolves directly from mutable authoring directories.
- [x] A session always runs from a pinned revision directory plus pinned runtime closure.
- [ ] Alias movement is the only way to change default active behavior.
- [x] Eval execution consumes the same global runtime permits as ordinary sessions.
- [ ] Layer mounts are pinned in `runtime.lock`, not rediscovered dynamically.
- [x] Promotion never mutates revision bytes in place.

## Delivery Order

1. Phase 0 must land first.
2. Phase 1 depends on Phase 0.
3. Phase 2 depends on Phase 1.
4. Phase 3 depends on Phases 1 and 2.
5. Phase 4 depends on the data models from Phases 1 to 3.

## Phase 0: Gateway Contract Simplification

Goal: remove gateway semantics that conflict with immutable revisions and generic approvals.

### P0-T01 Ingress strict targeting

- [x] Remove default target selection from ingress handling.
- [x] Reject missing `target` in all runtime entrypoints that launch or message an agent.
- [ ] Standardize target parsing so `agent_id` and `agent_ref` use one validation path.
- [ ] Reject malformed targets containing `@` instead of falling back to alias lookup.

### P0-T02 Config cleanup

- [x] Delete config fields that encode gateway-side default agent routing.
- [x] Delete config fields that encode install-specific approval policy.
- [x] Fail fast at startup if obsolete config keys are still present.

### P0-T03 Wake model collapse

- [ ] Reduce background wake reasons to timer and named signal.
- [x] Remove semantic wake heuristics such as stale-goal or retryable-failure inference.
- [ ] Remove scheduler branches that directly interpret prior tool output as a wake reason.

### P0-T04 Generic approval queue

- [ ] Introduce generic approval request, list, resolve, and cancel primitives.
- [ ] Route promotion-related gating through the generic approval queue.
- [ ] Ensure approval continuations resume against pinned session bindings rather than re-resolving targets.

### P0-T05 Role-specific gate removal

- [x] Remove role or agent-name checks from install, promotion, or evolution flows.
- [x] Remove any specialized-builder or evolution-steward fast paths.
- [x] Move any remaining policy branching to capability checks plus approval queue.

### P0-T06 Disclosure collapse

- [x] Reduce disclosure handling to restricted vs non-restricted output policy.
- [ ] Stop classifying disclosure by source path taxonomy.
- [ ] Ensure tool metadata drives output filtering decisions.
- [x] Map legacy manifest disclosure classes to the binary restricted flag during migration.
- [x] Keep reply filtering deterministic with one restricted-output redaction marker.

### Phase 0 exit checklist

- [x] `event.ingest` without `target` fails validation.
- [ ] Malformed targets containing `@` fail validation before alias lookup.
- [x] No gateway behavior depends on specific built-in agent ids.
- [ ] Scheduler wake logic only uses timer and named signal.
- [ ] Disclosure filtering no longer depends on multi-class path taxonomy.
- [ ] Approval continuation still works after the simplification.

## Phase 1: Revision Registry and Resolver

Goal: make immutable revisions plus explicit runtime closure the only execution unit.

### P1-T01 Revision and eval IDs

- [x] Add types for `AgentRef`, revision status, alias record, session binding, and promotion record.
- [x] Ensure `SessionAgentBinding` stores `requested_target` and nullable `alias_id`.
- [ ] Reserve stable ID formats exactly as defined in the spec.
- [x] Document and enforce MVP revision status semantics for `candidate`, `ready`, `rejected`, and `archived`.

### P1-T02 Gateway schema migration

- [ ] Replace ad hoc `CREATE TABLE IF NOT EXISTS` bootstrap with ordered schema migrations.
- [ ] Add `schema_migrations` metadata and startup version checks.
- [x] Create tables for revisions, aliases, session bindings, and promotion history.
- [x] Add uniqueness constraints for `(agent_id, content_digest)` and single alias per agent.
- [x] Make session binding nullable for `alias_id` and required for `requested_target`.

### P1-T03 Runtime closure model

- [x] Extend `runtime.lock` with pinned layer mounts.
- [ ] Define lock hashing over the normalized serialized runtime closure.
- [ ] Reject revision creation if artifact layers and lock layers do not agree.
- [ ] Write the normalized `runtime.lock` form into revision materialization.

### P1-T04 Agent bundle artifact kind

- [x] Add an explicit `AgentBundle` artifact kind.
- [ ] Validate that revision creation only accepts `AgentBundle` artifacts.
- [ ] Validate `SKILL.md`, manifest identity, and runtime lock presence before materialization.

### P1-T05 Revision materialization

- [ ] Materialize immutable revision directories under the revision store.
- [ ] Write revision metadata and status in one transactional flow.
- [ ] Ensure revision materialization is idempotent for the same content digest.

### P1-T06 Alias registry

- [x] Persist one mutable alias per logical agent.
- [x] Treat alias creation as part of promote, not part of revision creation.
- [ ] Expose alias listing and inspection as admin-safe operations.

### P1-T07 Resolver contract

- [x] Resolve explicit `agent_ref` before attempting alias lookup.
- [x] Return candidate revisions when an explicit `agent_ref` exists, even if no alias points to it.
- [ ] List aliases from registry state, not from authoring directories.

### P1-T08 Session binding and resume

- [x] Create session bindings before the first executable turn.
- [x] Persist `requested_target`, nullable `alias_id`, revision id, and runtime lock hash.
- [ ] Ensure approval resume, checkpoint resume, and retry paths always reload from binding state.

### P1-T09 Seeding flow

- [x] Add a deliberate seed flow: artifact -> revision create -> promote.
- [ ] Document that seeding is the only path to activate a new logical agent.
- [ ] Provide a test helper or admin command for deterministic seeding in integration tests.
- [ ] Replace CLI `agent.install` entrypoints with revision create plus promote flow.
- [ ] Document a first-deploy migration runbook for existing `agents/` authoring directories.

### P1-T10 Phase 1 tests

- [ ] Revision creation from an `AgentBundle` artifact succeeds.
- [ ] Duplicate revision creation reuses the same revision identity.
- [x] Explicit `agent_ref` resolution bypasses alias lookup.
- [x] Malformed targets containing `@` fail validation without alias fallback.
- [ ] Candidate revision can run without an alias.
- [ ] Changing pinned layer mounts changes revision identity even when agent files do not.
- [ ] Session resume reloads from stored binding state.
- [ ] Fresh and upgraded databases both pass ordered migration bootstrap.

### Phase 1 exit checklist

- [ ] New sessions execute only from revision directories.
- [ ] No runtime path depends on scanning authoring directories.
- [ ] Runtime closure is explicit and hashed.
- [ ] Ordered schema migrations replace ad hoc schema bootstrap.
- [ ] Candidate revisions are runnable by explicit `agent_ref`.
- [ ] Existing tests and operator seeding paths no longer depend on direct runtime directory loading.

## Phase 2: Promotion and Rollback

Goal: make alias movement the only activation and rollback mechanism.

### P2-T01 Promotion command

- [x] Implement promote with alias-to-agent validation.
- [x] Require revision existence and status in `candidate` or `ready`.
- [x] Support optional `required_eval_run_id` gating.

### P2-T02 Rollback command

- [x] Implement rollback to explicit target revision id.
- [x] Support rollback to the previous promoted revision when no target is supplied.
- [x] Validate same-agent lineage before alias movement.

### P2-T03 Atomic alias movement

- [x] Move alias target and write promotion history in one transaction.
- [x] Prevent partial success where history and alias diverge.
- [x] Ensure concurrent promote or rollback calls serialize correctly.

### P2-T04 Policy enforcement

- [ ] Gate promote and rollback through capability checks.
- [ ] Route any governance requirement through the generic approval queue.
- [ ] Keep policy independent from agent names or roles.

### P2-T05 Inspection surfaces

- [ ] Expose alias inspection as admin-safe CLI or HTTP surface.
- [ ] Expose promotion history inspection as admin-safe CLI or HTTP surface.
- [ ] Show which revision is active for each alias.

### P2-T06 Phase 2 tests

- [ ] Promote changes only future alias resolution.
- [ ] Running sessions stay pinned to the old revision.
- [ ] Explicit `agent_ref` sessions are unaffected by later promotion.
- [ ] Rollback restores previous alias target.
- [ ] Promotion fails for mismatched alias and agent.

### Phase 2 exit checklist

- [ ] Alias movement is the only activation path.
- [x] Promotion history is durable and auditable.
- [ ] Running sessions remain stable during promote and rollback.

## Phase 3: Eval Suite MVP

Goal: add measurable evidence before alias movement.

### P3-T01 Eval schema

- [x] Add suite, run, and case result types.
- [x] Add tables for suites, runs, and case results.
- [x] Define report handle persistence in the data model.

### P3-T02 Suite publish

- [x] Implement suite publication with stable `case_id` validation.
- [x] Validate the MVP assertion grammar.
- [x] Reject invalid suite specs before persistence.

### P3-T03 Eval queue

- [x] Implement durable eval run creation.
- [x] Add scheduler processing for queued eval runs.
- [x] Track queued, running, passed, failed, and cancelled states.

### P3-T04 Eval case execution

- [x] Launch each case with explicit `agent_ref`.
- [x] Create isolated eval session ids.
- [x] Persist outputs, notes, scores, and failure details per case.

### P3-T05 Assertion engine

- [x] Implement `reply_contains_all`.
- [x] Implement `reply_contains_none`.
- [x] Implement `reply_max_chars`.
- [x] Implement `artifacts_min` and `artifacts_max`.

### P3-T06 Report persistence

- [x] Aggregate run summary data.
- [x] Persist the full report to the content store.
- [x] Persist optional `baseline_ref` as report metadata only in MVP.
- [x] Expose summary plus `report_handle` through `eval.report`.

### P3-T07 Concurrency and permits

- [x] Ensure eval runs consume the same global execution permits as ordinary sessions.
- [x] Keep default per-run case concurrency at `1`.
- [x] Ensure multiple eval runs interleave only through the shared global permit pool.
- [x] Ensure eval workers do not reserve dedicated capacity away from interactive sessions.
- [x] Prevent eval queue execution from bypassing sandbox or spawn limits.

### P3-T08 Promotion integration

- [x] Allow promote to require a passed eval run.
- [x] Validate subject revision equality between promote request and eval run.
- [x] Preserve failed evals for inspection without making them promotable.

### P3-T09 Phase 3 tests

- [x] Eval run records are durable across restart.
- [x] Case failures produce failed run status and preserved report output.
- [ ] Promote fails when `required_eval_run_id` does not match the target revision.
- [x] Eval sessions against candidate revisions run with null `alias_id`.

### Phase 3 exit checklist

- [x] Eval runs are durable and inspectable.
- [x] Promotion can be gated by passed evidence.
- [x] Eval execution obeys ordinary runtime limits.

## Phase 4: Federation-Ready Provenance

Goal: make single-gateway behavior forward-compatible with exchange and import.

### P4-T01 Provenance fields everywhere

- [x] Ensure revision, promotion, eval suite, and eval run records all carry `origin_node_id`.
- [x] Ensure imported objects carry `trust_domain`, `source_kind`, and `source_ref`.
- [ ] Keep provenance mandatory rather than inferred.

### P4-T02 Capsule closure model

- [ ] Define future capsule manifest fields for `agent_ref` and pinned runtime closure.
- [ ] Reserve `included_layers` for hermetic export.
- [ ] Ensure capsule planning uses revision identity rather than directory naming.

### P4-T03 Import semantics

- [ ] Define candidate status for imported foreign revisions.
- [ ] Define trust-domain handling for imported revisions and eval artifacts.
- [ ] Define that import never auto-promotes a foreign object.

### P4-T04 Round-trip validation

- [ ] Serialize and deserialize provenance-bearing records without loss.
- [ ] Preserve provenance through export planning and import parsing.
- [ ] Verify foreign revisions remain distinguishable from local revisions.

### Phase 4 exit checklist

- [ ] Every durable record needed for future exchange has provenance.
- [ ] Capsule and import planning refer to revision identity plus runtime closure.
- [ ] No imported object is treated as implicitly trusted or active.

## Post-MVP Follow-ons from Archived Backlog

The archived backlog in [archived/plan_checkpoint_digest.md](archived/plan_checkpoint_digest.md) remains partly relevant, but it does not widen this MVP.

Use the following mapping when triaging older follow-on work:

- Keep workspace output capture for foreign agents, but track it under implicit artifact and output capture evolution rather than under revision or eval delivery. This MVP starts revision creation from an `AgentBundle` artifact; capture work is the bridge that can turn foreign sandbox outputs into shareable inputs for that path. See [spec-implicit-artifacts-agent-evolution.md](spec-implicit-artifacts-agent-evolution.md).
- Keep remote agent spawn as a post-MVP federation track. This plan stores provenance and capsule-ready metadata, but it does not deliver peer placement, leases, or cross-node live execution semantics.
- Keep autonomous agent export as a post-MVP federation track. The future export unit should package revision bytes, canonical runtime closure, included layers, and provenance for remote execution, and it must remain distinct from `AgentBundle`, which stays the revision-creation input.
- Keep post-session digest full end-to-end coverage only as separate QA debt if digest remains a strategic runtime feature. Digest behavior is not part of alias movement, revision pinning, or eval-gated promotion in this MVP.
- Do not carry response validation gate as open backlog in this plan. That capability already exists as a runtime feature; the relevant MVP work here is eval-time assertion and promotion evidence, not spawn-boundary response repair.
- Keep gateway tooling modularization as a post-MVP maintenance task. Split `autonoetic-gateway/src/runtime/tools.rs` into smaller topic-focused modules once the MVP tool surface stabilizes, so revision, eval, promotion, artifact, and workflow tools are easier to evolve and review independently.

This section is intentionally classification, not delivery scope. Only work that directly supports immutable revisions, alias-based activation, eval evidence, or federation-ready provenance belongs in the phases above.

## Recommended Merge Slices

1. Phase 0 ingress plus config cleanup.
2. Phase 0 scheduler plus approval queue cleanup.
3. Phase 1 types plus schema migration.
4. Phase 1 revision materialization plus resolver.
5. Phase 1 session binding plus seed flow.
6. Phase 2 promote plus rollback.
7. Phase 3 eval schema plus queue.
8. Phase 3 assertion engine plus report flow.
9. Phase 4 provenance plus capsule planning.

## Final Definition of Done

- [ ] A new logical agent is activated only through artifact -> revision -> promote.
- [x] A running session is fully determined by its stored binding.
- [x] A candidate revision can be evaluated before activation.
- [x] Alias movement is auditable and reversible.
- [ ] Runtime closure includes pinned layer mounts.
- [x] Provenance is present for later federation work.
