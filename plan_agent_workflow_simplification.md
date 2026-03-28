# Plan: Agent Workflow Simplification

## Goal

Reduce looping, approval confusion, and phase restarts across planner, coder, evaluator, and specialized_builder while keeping the gateway mechanically strict but semantically dumb.

## Constraints

- Gateway remains responsible for security, approvals, workflow bookkeeping, content storage, and resumable execution primitives.
- Gateway must not become a semantic planner.
- Agents remain responsible for deciding what to do next.
- Skills must become shorter, role-specific, and deterministic.

## Problems To Fix

1. Planner resumes by re-inferring state from long prose instead of reading a small explicit state contract.
2. Agent skills mix workflow mechanics, approval recovery, and semantic policy in the same prompt.
3. Approval and timeout instructions are duplicated and sometimes compete.
4. Specialists overreach into planning or recovery behavior that should stay outside their role.
5. The system lacks a compact, shared result contract between agent roles.

## Proposed Direction

### 1. Slim Skills Down To Policy

- Rewrite planner skill around resume algorithm, delegation rules, reuse guards, and output contract.
- Remove duplicated approval examples and timeout sections from planner.
- Move operational examples into docs where possible.
- Narrow coder, evaluator, and specialized_builder instructions so each role does one job only.

### 2. Add Explicit Agent-Owned Workflow State

Planner should maintain a small structured state object in writable storage, containing:

```json
{
  "goal_id": "...",
  "goal_summary": "...",
  "next_step": "architect|coder|evaluator|auditor|builder|done",
  "latest_artifact_id": "art_xxx|null",
  "latest_design_id": "impl_task_xxx|null",
  "blocked_on_approval_id": "apr_xxx|null",
  "active_task_ids": [],
  "invalidated_artifact_ids": []
}
```

Resume rule:

1. Read structured workflow facts from the gateway.
2. Read planner-owned state.
3. Reconcile differences.
4. Continue from `next_step`.
5. Only infer a fresh plan if `next_step` is absent.

### 3. Add Hard Reuse Guards In The Planner Skill

- If `latest_artifact_id` exists, planner must not respawn architect.
- If evaluator or auditor is awaiting approval, planner must not spawn coder or architect.
- If user says `continue` or `done`, planner must not reinterpret the original goal.
- If a successful coder result exists for the active goal and is not invalidated, planner must reuse it.

### 4. Standardize Specialist Output Envelopes

All specialist agents should return a compact structured envelope:

```json
{
  "status": "succeeded|failed|clarification_needed|awaiting_approval",
  "artifact_id": "art_xxx|null",
  "supersedes_artifact_id": "art_prev|null",
  "approval_request_id": "apr_xxx|null",
  "findings": [],
  "next_recommended_role": "coder.default|null",
  "invalidates_prior_outputs": false
}
```

This gives planner deterministic facts instead of prose interpretation.

### 5. Keep Gateway Dumb But Improve Workflow Facts

Add neutral primitives or data access patterns for:

- latest successful child output by role
- pending approvals for a root workflow
- active child tasks for a root workflow
- wake reason for a resumed continuation
- latest artifact ids produced in the workflow

The gateway should enforce only generic invariants:

- no bypass of approvals
- no use of missing artifacts
- no invalid continuation resume
- no install without required approval records
- no invalid parent/child task references

The gateway should not decide workflow semantics such as architect-before-coder.

## Implementation Phases

### Phase 1: Planner Refactor

1. Replace the current planner skill with a shorter deterministic version.
2. Remove duplicated timeout and approval sections.
3. Introduce explicit resume and restart-forbidden rules.

### Phase 2: Specialist Contract Tightening

1. Narrow coder to implementation and revision only.
2. Narrow evaluator to evaluation only.
3. Narrow specialized_builder to installation only.
4. Align all role outputs to a shared structured envelope.

### Phase 3: Workflow Fact Surface

1. Expose the minimum structured workflow facts agents need.
2. Avoid embedding semantic flow logic in the gateway.
3. Ensure resume events are visible and queryable.

### Phase 4: Validation

1. Exercise approval pause and resume flows.
2. Exercise `continue` after approval.
3. Verify planner reuses existing coder outputs instead of restarting.
4. Verify failed evaluator runs feed coder iteration instead of restarting architect.

## Success Criteria

- Planner does not restart from architect when a valid coder artifact already exists.
- Approval resolution resumes the blocked phase instead of rebuilding earlier phases.
- Specialists stop compensating for missing planner state with extra reasoning.
- Prompt size and duplicated instructions are materially reduced.
- The gateway remains a rules engine and workflow substrate, not a semantic orchestrator.