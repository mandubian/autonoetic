# Plan: Agent Workflow Simplification

## Goal

Reduce looping, approval confusion, and phase restarts across planner, coder, evaluator, and specialized_builder while keeping the gateway mechanically strict but semantically dumb.

The main objective is to make two Autonoetic mechanisms feel natural to agents:

1. Approval should behave like a paused operation that resumes, not like a new reasoning problem.
2. Child outputs should be handed off as implicit, discoverable results, not as guessed file names or explicit packaging rituals.

## Constraints

- Gateway remains responsible for security, approvals, workflow bookkeeping, content storage, and resumable execution primitives.
- Gateway must not become a semantic planner.
- Planner logic lives in its `SKILL.md`, not in hardcoded gateway workflow semantics.
- Planner may discover child agents dynamically or use agents built by Autonoetic itself.
- Nothing about the available child agents is guaranteed ahead of time beyond a few common-sense rules.
- Agents remain responsible for deciding what to do next.
- Skills must become shorter, role-specific, and deterministic.

## Problems To Fix

1. Approval is currently experienced by agents as an interruption or error path rather than as a continuation of the same operation.
2. Child outputs and artifacts are exposed in a way that is unnatural for ordinary agents, which naturally think in files, outputs, and results rather than artifact packaging.
3. Planner resumes by re-inferring state from long prose instead of from compact structured workflow facts.
4. Agent skills mix workflow mechanics, approval recovery, and semantic policy in the same prompt.
5. Approval and timeout instructions are duplicated and sometimes compete.
6. Specialists overreach into planning or recovery behavior that should stay outside their role.
7. The system lacks a compact, shared result contract between agent roles.
8. Existing role chains assume known agents in advance, but the planner may need to discover or select agents at runtime.

## Proposed Direction

### 0. Make Mechanisms Feel Native Before Adding More Orchestration

Before adding more planner policy, reduce friction in the two runtime mechanisms that currently force agents into unnatural behavior.

- Approval should look like suspend/resume, not fail/rethink/retry.
- Outputs should look like stable task results, not guessed names or packaging chores.
- Planner simplification should follow from these contracts, not compensate for their rough edges.

If these contracts remain awkward, additional planner logic will likely increase prompt complexity without fixing the root cause.

### 1. Approval Must Be A Continuation Contract

The approval mechanism is imposed by the gateway, so the gateway must expose it in the most agent-natural form possible.

Target behavior:

- Agent calls a tool.
- Tool is suspended for approval.
- After approval, the same operation resumes.
- Agent receives a canonical continuation result and continues.
- Agent should not need to reconstruct the workflow from scratch.

Design implications:

- `approval_required` should be treated as suspension, not ordinary failure.
- Resumed context should be canonical, explicit, and minimal.
- Resume semantics should be universal across agents rather than repeated in large role-specific prompt sections.
- Approval-specific retry rules should stay secondary to the continuation model.

Acceptance test:

- A competent but not highly specialized agent should be able to continue correctly after approval with only a short universal resumption rule.

### 2. Outputs Must Be Implicit And Discoverable

Most agents naturally work in files and final outputs. They do not naturally reason in explicit artifact packaging terms.

Target behavior:

- Child agent completes work.
- Gateway creates or surfaces a stable implicit output handle.
- Parent sees this handle directly in completion results.
- Parent consumes the handle without guessing names or reconstructing packaging details.

Design implications:

- Ordinary agents should think in terms of outputs, files, and result handles.
- Implicit artifacts should serve as the transport and persistence substrate.
- Explicit artifact handling should remain mainly for roles where packaging boundaries truly matter, such as evaluation and installation.

Acceptance test:

- Parent agents should not need to guess names like `weather_result` or manually infer output locations from task ids.

### 3. Slim Skills Down To Policy

- Rewrite planner skill around resume algorithm, discovery rules, delegation guards, and output contract.
- Remove duplicated approval examples and timeout sections from planner.
- Move operational examples into docs where possible.
- Narrow common specialist instructions so each role does one job only.

### 4. Prefer Structured Workflow Metadata Over Mutable Planner State

Do not rely on free-form planner memory or optional writes like `knowledge.store` and `content.write` for workflow control.

Instead, each important workflow action should carry a required structured metadata envelope that the gateway persists mechanically with the task.

Illustrative planner-to-child spawn metadata:

```json
{
  "goal_id": "...",
  "goal_summary": "...",
  "phase_family": "design|implement|evaluate|audit|install|research|debug|other",
  "strategy_id": "default_implementation|fix_after_evaluation|alternative_design|install|...",
  "attempt_fingerprint": "hash_of_material_inputs",
  "parent_artifact_id": "art_xxx|null",
  "parent_result_id": "task_or_content_handle|null",
  "expected_outcome": "artifact|report|recommendation|installation|clarification",
  "reply_to_agent_id": "planner.default"
}
```

Resume rule:

1. Read structured workflow facts from the gateway.
2. Reconstruct the current workflow situation from persisted task metadata, child outputs, approvals, and artifacts.
3. Continue from the latest unresolved step.
4. Only infer a fresh plan when the workflow has no usable structured prior state.

Iteration rule:

- Track retries by `phase_family` plus `attempt_fingerprint`, not by concrete agent name alone.
- Do not increment on passive wake-ups such as approval resolution or child completion unless planner explicitly retries a materially equivalent attempt.
- If the same `phase_family` and `attempt_fingerprint` exceed the retry limit, planner must stop automatic retries, summarize the repeated failure, and ask the user whether to continue or change strategy.
- A materially different attempt may reset the per-fingerprint counter while still counting against a broader family budget.
- Suggested initial family limits: design `1`, implement `2`, evaluate `2`, audit `1`, install `1`.

Discovery rule:

- Planner must not assume a fixed set of children beyond generic families such as design, implement, evaluate, audit, install, research, and debug.
- Planner may discover agents dynamically or use newly built agents, but must first classify them into a generic family before using them automatically.
- Unknown or unclassified agents should not be selected automatically for critical workflow steps.

### 3. Add Hard Reuse Guards In The Planner Skill

- If a usable artifact already exists for the current goal and no new material input invalidates it, planner must prefer reuse over re-design.
- If a child in the current attempt family is awaiting approval, planner must not spawn another materially equivalent child.
- If user says `continue` or `done`, planner must not reinterpret the original goal.
- If a successful child result exists for the active goal and is not invalidated, planner must reuse it.
- If the retry counter for the current attempt fingerprint is exhausted, planner must not loop automatically into the same attempt again.
- If the planner changes agents, it must be because the strategy or material inputs changed, not because the same retry is being renamed.

### 4. Standardize Specialist Output Envelopes

All child agents should return a compact structured envelope:

```json
{
  "status": "succeeded|failed|clarification_needed|awaiting_approval",
  "phase_family": "design|implement|evaluate|audit|install|research|debug|other",
  "agent_id": "agent_name",
  "artifact_id": "art_xxx|null",
  "supersedes_artifact_id": "art_prev|null",
  "approval_request_id": "apr_xxx|null",
  "failure_signature": "approval_pending|approval_timeout|artifact_missing|test_failed|install_rejected|...|null",
  "findings": [],
  "next_recommended_family": "implement|evaluate|audit|install|research|debug|other|null",
  "invalidates_prior_outputs": false
}
```

This gives planner deterministic facts instead of prose interpretation.

### 5. Keep Gateway Dumb But Improve Workflow Facts

Add neutral primitives or data access patterns for:

- child tasks and their persisted metadata for a root workflow
- latest successful child output by family or agent id
- pending approvals for a root workflow
- active child tasks for a root workflow
- wake reason for a resumed continuation
- latest artifact ids produced in the workflow
- prior attempts sharing the same `attempt_fingerprint`

The gateway should enforce only generic invariants:

- no bypass of approvals
- no use of missing artifacts
- no invalid continuation resume
- no install without required approval records
- no invalid parent/child task references
- persisted task metadata must be well-formed if provided

The gateway should not decide workflow semantics such as architect-before-coder or whether a newly discovered agent is a valid next step.

### 6. Keep Explicit Artifacts As A Specialist Boundary, Not A Universal Cognitive Burden

Explicit artifacts are still useful, but they should be required mainly where a closed boundary matters.

- Evaluators may need explicit artifact ids for reproducible validation.
- Builders/installers may need explicit artifact ids for packaging and installation.
- Ordinary agents should usually interact with implicit outputs and result handles first.

This preserves the artifact model where it is valuable without forcing all agents to think like packagers.

## Implementation Phases

### Phase 1: Mechanism Hardening

1. Tighten the approval continuation contract so resumed tool calls feel like continuation, not restart.
2. Tighten implicit output handoff so completed child tasks always surface stable, discoverable output handles.
3. Validate that ordinary agents can resume and consume outputs without large specialized prompt instructions.

### Phase 2: Planner Refactor

1. Replace the current planner skill with a shorter deterministic version built around discovery, classification, reuse, and retry guards.
2. Remove duplicated timeout and approval sections.
3. Introduce explicit resume and restart-forbidden rules.
4. Introduce attempt fingerprints, family retry budgets, and stop conditions.

### Phase 3: Specialist Contract Tightening

1. Narrow common known specialists such as coder, evaluator, and specialized_builder to their core responsibilities.
2. Align all child outputs to a shared structured envelope.
3. Ensure dynamically discovered agents can still participate if they classify cleanly into a known family.

### Phase 4: Workflow Fact Surface

1. Expose the minimum structured workflow facts agents need.
2. Avoid embedding semantic flow logic in the gateway.
3. Ensure resume events are visible and queryable.
4. Expose prior attempts by fingerprint so loop detection is based on workflow history, not LLM memory.

### Phase 5: Validation

1. Exercise approval pause and resume flows.
2. Exercise `continue` after approval.
3. Verify parent agents consume implicit outputs without guessing names.
4. Verify planner reuses existing child outputs instead of restarting equivalent work.
5. Verify failed evaluation runs feed a changed implementation attempt instead of restarting equivalent work.
6. Verify planner can discover a new child agent, classify it, and use it without a predetermined hardcoded route.

## Success Criteria

- Approval no longer feels like a separate reasoning branch to ordinary agents.
- Parent agents consume stable child outputs without guessing names or reconstructing packaging details.
- Planner does not restart equivalent prior work when a valid reusable output already exists.
- Approval resolution resumes the blocked phase instead of rebuilding earlier phases.
- Specialists stop compensating for missing planner state with extra reasoning.
- Prompt size and duplicated instructions are materially reduced.
- The planner stops and asks for direction instead of silently looping once an equivalent attempt exceeds its retry budget.
- The planner can discover and use new agents without hardcoding them into a fixed workflow chain.
- Explicit artifact handling remains where it matters, without becoming a universal burden for every agent.
- The gateway remains a rules engine and workflow substrate, not a semantic orchestrator.

## Concrete Refactoring Checklist

This checklist targets the current codebase directly. The aim is to tighten the two main mechanism contracts first, then simplify agent skills around those stronger runtime guarantees.

### A. Approval Continuation Contract ✅

1. ✅ Refactored `autonoetic-gateway/src/scheduler/approval.rs` so approval resolution emits a canonical continuation payload (`approval_resumed:<action>:<request_id>:<status>`) for both `sandbox.exec` and install flows.
2. ✅ Removed human-oriented retry prose in `resume_session_after_approval()` — replaced with structured machine-readable continuation strings.
3. ✅ `unblock_task_on_approval()` checkpoint writes now use structured fields (`approval_resolved`, `request_id`, `status`, `action_type`) instead of free-form `resume_message`.
4. ✅ Normalized `approval_resolved` checkpoint payload shape so every resumed tool flow exposes the same keys.
5. ✅ Approval rejection and approval timeout paths share the same continuation contract shape, differing only by status and reason.

### B. Turn Continuation Runtime ✅ (no changes needed)

1. ✅ Reviewed `autonoetic-gateway/src/runtime/continuation.rs` — already carries minimum structured state. The `TurnContinuation` struct captures full history, pending tool call, completed results, and remaining calls. Resume is fully mechanical.
2. ✅ `TurnContinuation` fields are appropriate — no fields force agents to infer from chat history since `execute_approved_action()` and `reconstruct_history()` handle everything mechanically.
3. ✅ `execute_approved_action()` is already a pure mechanical executor with no semantic branching logic.
4. ✅ Existing tests cover continuation save/load/delete semantics.

### C. Sandbox Approval UX ✅

1. ✅ Refactored `SandboxExecTool` in `autonoetic-gateway/src/runtime/tools.rs` so approval-required responses use suspension-first framing (`"suspended": true`).
2. ✅ Messages now say "Execution suspended pending operator approval. The approved command is persisted and will be used automatically on resume." instead of "retry with approval_ref".
3. ✅ Removed wording that makes agents think they must reconstruct or re-plan the blocked command.
4. ✅ Both the fresh approval path and the duplicate pending-approval path return the same structured fields.

### D. Implicit Output Handoff ✅

1. ✅ Updated `workflow.wait` description to explicitly document the `output` field as the canonical parent-child output handoff mechanism.
2. ✅ Added `workflow.state` tool that returns structured workflow facts (completed tasks, pending approvals, reuse guards, resume hint) in one call.
3. ✅ Updated `ContentReadTool` hint behavior so guessed-name failures point to `workflow.wait`/`workflow.state` as the canonical way to discover output handles.
4. ✅ The implicit output path is now the documented default for ordinary agents.

### E. Explicit Artifact Boundary ✅

1. ✅ Reframed `ArtifactBuildTool` description: "Artifacts are specialist-boundary objects... For ordinary parent-child output handoff, prefer the implicit output from workflow.wait."
2. ✅ Artifacts remain first-class for evaluation, installation, and closed-boundary execution.
3. ✅ Reframed `ArtifactInspectTool` similarly.
4. ✅ `content.read` and `artifact.inspect` descriptions now make the implicit-vs-explicit split unambiguous.

### F. Workflow Types And Event Surface ✅ (no changes needed)

1. ✅ Reviewed `autonoetic-types/src/workflow.rs` — `WorkflowRun`, `TaskRun`, `TaskCheckpoint`, `WorkflowCheckpoint`, and `WorkflowEventRecord` already expose sufficient structure.
2. ✅ The `output` field on succeeded `TaskRun` entries (populated by `workflow.wait`) provides the implicit artifact handoff.
3. ✅ No role-specific semantics in shared workflow types.

### G. Chat And Operator Visibility ⚠️ (deferred — low priority)

1. ⚠️ `autonoetic/src/cli/chat.rs` still shows approval events with the old wording. The signal messages have been updated but the chat display layer has not been audited.
2. ⚠️ Event cards should emphasize: what was paused, whether it resumed, and which stable output or approval id is relevant next.
3. ⚠️ Keep the chat surface aligned with the gateway contract so agents and operators see the same mental model.

### H. Planner Skill Simplification ✅

1. ✅ Rewrote `agents/lead/planner.default/SKILL.md` (554 → ~430 lines) after runtime contracts were tightened.
2. ✅ Removed duplicated timeout and approval sections.
3. ✅ Replaced 128-line recovery prose with a short universal rule: "Call `workflow.state` on wake-up. Read `resume_hint` and `reuse_guards`. Continue from where the workflow left off."
4. ✅ Replaced artifact/name-guessing guidance with: "Use `workflow.state` for structured facts. Use `workflow.wait` output handles for child results."
5. ✅ Discovery-driven delegation preserved: planner classifies agents into generic families.
6. ⚠️ Attempt fingerprints and family retry budgets are documented in the plan but not yet implemented as gateway-enforced metadata (deferred — the skill-level reuse guards handle the common cases).

### I. Common Specialist Skill Cleanup ✅

1. ✅ Rewrote `agents/specialists/coder.default/SKILL.md` (368 → ~210 lines) — approval handling reduced to a 4-step universal resumption rule.
2. ✅ Rewrote `agents/specialists/evaluator.default/SKILL.md` (340 → ~195 lines) — consumes explicit artifacts only for validation boundaries.
3. ✅ Rewrote `agents/specialists/builder.default/SKILL.md` (226 → ~150 lines) and `agents/evolution/specialized_builder.default/SKILL.md` (258 → ~175 lines) — installation remains artifact-aware but resumption is minimal.
4. ✅ Removed all instructions that made specialists compensate for weak runtime contracts with extra reasoning.

### J. Tests To Update Or Add ✅

1. ✅ Updated `workflow_approval_resume_integration` test to assert the canonical continuation payload shape (`approval_resumed:sandbox_exec:apr-xxx:approved`).
2. ✅ The existing continuation mechanism already behaves like continuation of the original call (handled by `execute_approved_action`).
3. ✅ `workflow.wait` already returns stable implicit output handles for completed child tasks (existing behavior, now better documented).
4. ✅ `content.read` guessed-name failures now include canonical implicit-output hints.
5. ⚠️ End-to-end parent/child implicit output test deferred (the existing integration tests cover the component behaviors).

### K. Documentation Sync ⚠️ (deferred — low priority)

1. ⚠️ `docs/spec-implicit-artifacts-agent-evolution.md` should be updated to clearly separate ordinary-agent output handoff from specialist explicit-artifact workflows.
2. ⚠️ Docs covering approval retry and checkpoints should describe the system as suspend/resume continuation first, retry mechanics second.
3. ✅ Planner and specialist skill docs are already minimal after the skill rewrites.

## Summary of Changes

### Files Modified

| File | Change |
|------|--------|
| `autonoetic-gateway/src/scheduler/approval.rs` | Canonical continuation payload format; removed prose retry messages |
| `autonoetic-gateway/src/scheduler/workflow_store.rs` | Updated `refresh_queued_task_message_from_task_checkpoint` for new checkpoint format |
| `autonoetic-gateway/src/runtime/tools.rs` | Added `WorkflowStateTool`; updated `SandboxExecTool` approval framing; updated `workflow.wait`, `content.read`, `artifact.build`, `artifact.inspect` descriptions |
| `agents/lead/planner.default/SKILL.md` | 554 → ~430 lines; `workflow.state`-based resume; hard reuse guards |
| `agents/specialists/coder.default/SKILL.md` | 368 → ~210 lines; universal resumption rule |
| `agents/specialists/evaluator.default/SKILL.md` | 340 → ~195 lines; narrowed to evaluation only |
| `agents/specialists/builder.default/SKILL.md` | 226 → ~150 lines; streamlined |
| `agents/evolution/specialized_builder.default/SKILL.md` | 258 → ~175 lines; streamlined |
| `autonoetic-gateway/tests/workflow_approval_resume_integration.rs` | Updated for canonical continuation format |

### Lines of Skill Text Removed

| Skill | Before | After | Reduction |
|-------|--------|-------|-----------|
| Planner | 554 | ~430 | -124 (22%) |
| Coder | 368 | ~210 | -158 (43%) |
| Evaluator | 340 | ~195 | -145 (43%) |
| Builder | 226 | ~150 | -76 (34%) |
| Specialized Builder | 258 | ~175 | -83 (32%) |
| **Total** | **1,746** | **~1,160** | **-586 (34%)** |
