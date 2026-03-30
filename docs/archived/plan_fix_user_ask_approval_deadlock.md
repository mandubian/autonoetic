# Plan: Fix `user.ask` / Approval / Workflow Deadlock

## Problem

The planner can call `user.ask` while orchestrating child tasks, creating a deadlock:

1. Child task hits approval → suspended
2. Planner calls `user.ask` instead of `workflow.wait` → session checkpoints with `UserInputRequired`
3. Session closes as `jsonrpc_spawn_complete_empty` (misleading — looks "done" not "blocked")
4. Approval resolves → child completes → workflow join satisfied → signal sent to planner
5. Planner session can't resume (blocked on unanswered `user.ask` interaction)
6. Workflow is stranded

## Root Causes

1. **No runtime guard**: `user.ask` is legal in any state, including during active workflow orchestration
2. **Misleading outcome**: `user.ask` returns `TurnOutcome::Completed(None)` → `jsonrpc_spawn_complete_empty` instead of a true suspension state
3. **Skill-only enforcement**: The skill says "DO NOT call `user.ask` for approvals" but the LLM can still violate this

## Solution: 2-Layer Defense

### Layer 1: Runtime Guard on `user.ask` (Primary Fix)

**File: `autonoetic-gateway/src/runtime/tools.rs`** — `UserAskTool::execute()`

Before creating the interaction, check the root workflow state:

```rust
let root_sid = root_session_id(session_id);

// Check for active child tasks in this root session's workflow
let workflow_id = resolve_workflow_id_for_root_session(config, &root_sid)?;
if let Some(wf_id) = &workflow_id {
    let task_runs = list_task_runs_for_workflow(config, store, wf_id)?;
    let has_active_children = task_runs.iter().any(|t| {
        matches!(t.status,
            Pending | Runnable | Running | AwaitingApproval | Paused
        )
    });
    if has_active_children {
        return Err("user.ask is not available while workflow tasks are active. Use workflow.wait.");
    }
}

// Check for pending approvals for this root session
let pending = store.get_pending_approvals_for_root(&root_sid)?;
if !pending.is_empty() {
    return Err("user.ask is not available while approvals are pending.");
}
```

**Rule:** Reject `user.ask` for ANY session under an active workflow root — not just the planner session. This prevents child agents from accidentally using `user.ask` during orchestration too.

### Layer 2: True Suspension Outcome (Already Implemented)

`TurnOutcome::SuspendedUserInput` and `jsonrpc_spawn_suspended_user_input` close reason already exist in the codebase. The session is now visibly "suspended on user input" rather than "completed empty".

### Layer 3: Task ID in Approval Signal Delivery (Continuation Resume Fix)

**File: `autonoetic-gateway/src/router.rs`** — `spawn_agent_once()`

When `event.ingest` delivers an approval signal, extract `task_id` from the approval request so the continuation file can be found:

```rust
// Extract workflow_id and task_id from metadata when this is an approval signal
let (workflow_id, task_id) = metadata
    .and_then(|m| {
        let approval_id = m.get("approval_request_id")?.as_str()?;
        let store = self.execution.gateway_store()?;
        let approval = store.get_approval(approval_id).ok()??;
        Some((approval.workflow_id, approval.task_id))
    })
    .unwrap_or((None, None));
```

This ensures that after approval, the evaluator session resumes from the **turn continuation** (with real `sandbox.exec` results injected into the conversation) rather than starting fresh and re-deriving what to do.

### Layer 4: Artifact-Level Approval Reuse (Cross-Session)

**File: `autonoetic-gateway/src/runtime/tools.rs`** — `SandboxExecTool` artifact approval check

When an artifact requires network access approval:

1. **Check if already approved at root level**: If any `ApprovalRequest` for the same artifact (matched by artifact ID in the `reason` field) has `status == Approved` at the root session level, skip approval entirely. Artifacts are immutable — one approval covers all sessions.

2. **Check root-level pending approvals**: Before creating a new approval, check if any session under the same root already has a pending approval for this artifact. Reuse it instead of minting a duplicate.

3. **Fall back to session-level check**: Only create a new approval if no root-level or session-level pending approval exists.

This prevents the evaluator from needing a separate approval when the coder's artifact was already approved.

### Layer 5: Skill Text (Defense in Depth)

**File: `agents/lead/planner.default/SKILL.md`** — Already updated:
- "DO NOT call `user.ask`" for approval handling
- Parallel delegation tightened: agent creation is "STRICTLY SEQUENTIAL"

## What Was NOT Done (Intentionally)

**No auto-resume on workflow signals.** Auto-answering a pending `user.ask` with a synthetic "proceed" when a workflow join arrives would fabricate human intent and collapse two distinct semantics (workflow progress vs explicit user input). The current runtime correctly requires a real answered interaction before resuming (`resume_from_user_interaction` checks `status == Answered`). Layer 1 makes this moot anyway — if `user.ask` can't be called during active orchestration, the deadlock can't happen.

## Files Modified

1. `autonoetic-gateway/src/runtime/tools.rs` — Runtime guard in `UserAskTool::execute()` + artifact-level approval reuse in `SandboxExecTool`
2. `autonoetic-gateway/src/runtime/lifecycle.rs` — `TurnOutcome::SuspendedUserInput` (already present)
3. `autonoetic-gateway/src/execution.rs` — `jsonrpc_spawn_suspended_user_input` close reason (already present)
4. `autonoetic/src/cli/agent.rs` — CLI handling for `SuspendedUserInput` (already present)
5. `autonoetic-gateway/src/router.rs` — Extract `task_id` from approval signal metadata for continuation resume
6. `agents/lead/planner.default/SKILL.md` — Skill text tightening (already present)

## Testing

- All gateway tests pass (including `user_interaction_resume_integration` and `turn_continuation_approval_integration`)
- `user.ask` with active children → returns error JSON
- `user.ask` with idle workflow → creates interaction normally
- Approval signal delivery includes `task_id` → continuation resume works correctly
- Artifact approval is reused across sessions under the same root workflow
