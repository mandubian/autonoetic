# Plan: Rewrite Workflow Approval as Turn Continuation

## Problem Statement

When an agent tool call (`sandbox.exec`, `agent.install`) requires operator approval, the current system **kills the agent turn** and relies on the agent to understand a text notification and retry the tool with `approval_ref` on resume. This breaks agent continuity:

- The coder agent loses its mid-task reasoning (write code → exec → **approval wall** → resume → forgets to create artifact → planner retries from scratch → loop).
- Two different resume message formats exist (plain text signal vs JSON checkpoint), confusing the agent.
- The agent must parse a natural-language instruction to retry with `approval_ref` — LLMs frequently fail to do this correctly.
- ~400 lines of complex state management (checkpoint dance, Runnable re-queue, resume message construction) exist solely to work around this.

## Design: Turn Continuation Model

Replace the "kill turn + notify + agent retry" pattern with **checkpoint-and-resume**: save the turn state when approval is needed, release all resources, and seamlessly resume the turn when approval arrives — injecting the real tool result so the LLM never knows approval happened.

### Core Principle

The approval boundary becomes invisible to the agent. From the LLM's perspective, the tool call was slow but returned a result. The agent continues its plan uninterrupted.

### Architecture

```
Agent turn executes normally:
  LLM → tool_call(sandbox.exec) → tool handler detects remote access
    → returns {"approval_required": true, "request_id": "apr-xxx"}

Turn loop (lifecycle.rs) intercepts:
  1. Save TurnContinuation to disk (history + pending tool call + partial results)
  2. Return SuspendedForApproval to caller
  3. Task status → AwaitingApproval
  4. Release tokio task, release claim — zero resources held

... operator approves (minutes/hours later) ...

Scheduler tick detects resolved approval:
  1. Task status → Runnable (as today)
  2. process_runnable_workflow_tasks re-queues (as today)
  3. New tokio task spawned → spawn_task_execution → execute_with_history

execute_with_history detects continuation:
  1. Load TurnContinuation from disk
  2. Check approval decision in SQLite
  3. Gateway executes the approved action directly (sandbox.exec / agent.install)
  4. Reconstruct history: [...saved_history, assistant(tool_calls), tool_result(REAL output)]
  5. Delete continuation file
  6. Enter main loop — LLM sees its tool call and the real result
  7. Agent continues: checks output → creates artifact → done
```

### What Changes for Future Primitives

Any new tool that requires external approval follows the same pattern:
1. Tool handler returns `{"approval_required": true, "request_id": "..."}` with a `ScheduledAction` variant storing the full payload
2. Turn loop saves continuation and suspends — no tool-specific logic needed
3. `execute_approved_action()` dispatches by `ScheduledAction` variant — add one match arm per new primitive
4. Done. No new resume message formats, no agent retry logic, no checkpoint dance.

---

## Spec: TurnContinuation Type

**New file:** `autonoetic-gateway/src/runtime/continuation.rs`

```rust
/// Serializable snapshot of an agent turn suspended at an approval boundary.
/// Saved to disk when a tool requires approval; loaded on resume to continue
/// the turn seamlessly with the real tool result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnContinuation {
    /// Full conversation history at the point of suspension.
    /// Includes system message, user message, and all prior assistant/tool exchanges.
    pub history: Vec<Message>,

    /// The assistant message containing the tool call(s) that triggered approval.
    /// On resume, this is appended to history before the real tool result.
    pub assistant_message: Message,

    /// Tool results already collected in this batch BEFORE the approval-requiring one.
    /// These are injected as tool_result messages before the approval tool result.
    pub completed_tool_results: Vec<(String, String, String)>, // (call_id, tool_name, result_json)

    /// The specific tool call that requires approval.
    pub pending_tool_call: PendingApprovalToolCall,

    /// Tool calls that were NOT processed because they came after the approval one.
    /// On resume, these are re-executed after the approval result is injected.
    pub remaining_tool_calls: Vec<ToolCall>,

    /// Approval request ID in GatewayStore.
    pub approval_request_id: String,

    /// Workflow context for task status management.
    pub workflow_id: Option<String>,
    pub task_id: Option<String>,

    /// Session and turn identifiers for correlation.
    pub session_id: String,
    pub turn_id: String,

    /// Timestamp of suspension (for timeout calculation).
    pub suspended_at: String, // RFC3339

    /// Loop guard state at suspension (iteration count, failure count).
    pub loop_guard_state: LoopGuardState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApprovalToolCall {
    pub call_id: String,
    pub tool_name: String,
    pub arguments: String,
    /// The raw approval_required response from the tool handler.
    pub approval_response: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopGuardState {
    pub iterations: usize,
    pub failures: usize,
}
```

**Storage:** `.gateway/continuations/{task_id}.json` (one file per suspended task).

**Functions:**
```rust
pub fn save_continuation(config: &GatewayConfig, task_id: &str, cont: &TurnContinuation) -> Result<()>;
pub fn load_continuation(config: &GatewayConfig, task_id: &str) -> Result<Option<TurnContinuation>>;
pub fn delete_continuation(config: &GatewayConfig, task_id: &str) -> Result<()>;
pub fn list_suspended_task_ids(config: &GatewayConfig) -> Result<Vec<String>>;
```

---

## Spec: execute_approved_action

**New function in:** `autonoetic-gateway/src/runtime/continuation.rs`

Dispatches the approved action and returns the tool result the agent would have received.

```rust
pub fn execute_approved_action(
    decision: &ApprovalDecision,
    manifest: &AgentManifest,
    agent_dir: &Path,
    gateway_dir: Option<&Path>,
    config: &GatewayConfig,
    gateway_store: Option<Arc<GatewayStore>>,
) -> anyhow::Result<String> {
    match &decision.action {
        ScheduledAction::SandboxExec { command, dependencies, .. } => {
            // Execute the sandbox command directly using the same sandbox driver
            // that sandbox.exec uses, but with approval already validated.
            // Returns: {"ok": true/false, "exit_code": N, "stdout": "...", "stderr": "..."}
            execute_sandbox_command(command, dependencies, manifest, agent_dir, config)
        }
        ScheduledAction::AgentInstall { payload, .. } => {
            // Re-invoke the agent.install handler with the stored payload
            // and the approval_ref pre-validated.
            // Returns: {"ok": true, "agent_id": "...", ...}
            execute_agent_install(payload, &decision.request_id, manifest, agent_dir, gateway_dir, config, gateway_store)
        }
        // Future primitives: add match arms here
    }
}
```

This function reuses the existing sandbox execution and agent install code paths but bypasses the approval gate (since approval is already validated).

---

## Spec: Approval Timeout

**New config field in** `autonoetic-types/src/config.rs`:

```rust
/// Maximum time (seconds) a task can remain in AwaitingApproval before being
/// automatically failed. 0 = no timeout (not recommended for production).
/// Default: 600 (10 minutes).
pub approval_timeout_secs: u64,
```

**Enforcement:** In the scheduler tick, after processing background agents and before processing runnable/queued tasks:

```rust
fn check_approval_timeouts(config, store) {
    for task_id in list_suspended_task_ids(config)? {
        let cont = load_continuation(config, &task_id)?;
        let elapsed = now - parse_rfc3339(&cont.suspended_at);
        if elapsed > Duration::from_secs(config.approval_timeout_secs) {
            // Timeout: mark task as Failed
            update_task_run_status(config, store, &cont.workflow_id, &task_id,
                TaskRunStatus::Failed,
                Some("Approval timed out".to_string()));
            delete_continuation(config, &task_id)?;
            // Join check will fire in update_task_run_status → planner notified
        }
    }
}
```

---

## Spec: Task Cancellation

**New tool:** `workflow.cancel_task`

```rust
// Tool definition
{
    "name": "workflow.cancel_task",
    "description": "Cancel a task that is AwaitingApproval or Pending. Running tasks cannot be cancelled (use timeout instead).",
    "parameters": {
        "task_id": { "type": "string", "description": "Task to cancel" },
        "reason": { "type": "string", "description": "Why the task is being cancelled" }
    }
}
```

**Implementation:**
1. Load task → verify status is `AwaitingApproval` or `Pending`
2. Delete continuation file if exists
3. `update_task_run_status(Cancelled, reason)` → triggers join check
4. Return `{"ok": true, "task_id": "...", "status": "Cancelled"}`

Cancelling `Running` tasks is deferred to a later iteration (requires cooperative cancellation via a flag checked in the turn loop).

---

## Spec: Cleanup of Removed Code

### Code to remove

| What | File | Lines (approx) | Why |
|---|---|---|---|
| `approval_required_force_end_turn` flag | `lifecycle.rs` | 288, 313-315, 570-593 | No longer forcing EndTurn on approval |
| "CRITICAL: end your turn" system message injection | `lifecycle.rs` | 584-593 | Agent never sees approval |
| `build_approval_resume_message()` | `approval.rs` | 346-423 (~78 lines) | No resume message needed |
| `store_approval_result_in_checkpoint()` | `approval.rs` | 425-472 (~48 lines) | Continuation replaces this |
| `resume_session_after_approval()` for workflow-bound tasks | `approval.rs` | 151-323 (~170 lines) | Task resumes via continuation, not signal |
| `should_resume_waiting_session()` branching | `approval.rs` | 86-103, 337-339 | All tasks use continuation model |
| `process_runnable_workflow_tasks()` resume message logic | `scheduler.rs` | 601-618 | No resume message extraction needed |
| Stale queued task reconciliation | `scheduler.rs` | 620-654 | Continuation model is simpler |
| `TaskRunStatus::Runnable` (eventually) | `workflow.rs` | — | May still be useful as intermediate state; evaluate after implementation |
| `inject_resumption_context()` | `tool_call_processor.rs` | 273-301 | No longer resuming with synthetic context |
| `is_resumption` flag and logic | `tool_call_processor.rs` | 30, 72-81, 134-139 | Not needed for continuation model |
| `pending_approval_ids` field on `WorkflowRun` | `workflow.rs` | 68 | Never populated, dead code |
| `blocked_task_ids` field on `WorkflowRun` | `workflow.rs` | — | Never populated, dead code |

**Estimated net removal:** ~350-400 lines across `approval.rs`, `lifecycle.rs`, `scheduler.rs`, `tool_call_processor.rs`.

### Code to keep (still needed)

| What | File | Why |
|---|---|---|
| `approve_request()` / `reject_request()` entry points | `approval.rs` | CLI calls these; they now just record decision + set task Runnable |
| `decide_request()` | `approval.rs` | Records decision in SQLite, logs causal event |
| `unblock_task_on_approval()` | `approval.rs` | Transitions task AwaitingApproval → Runnable |
| `process_runnable_workflow_tasks()` | `scheduler.rs` | Still re-queues Runnable tasks, but without resume message extraction |
| `process_queued_workflow_tasks()` + `spawn_task_execution()` | `scheduler.rs` | Unchanged — spawns tasks as today |
| `update_task_run_status()` + join check | `workflow_store.rs` | Unchanged |
| Workflow events (`task.awaiting_approval`, `task.approved`) | `workflow_store.rs` | Still emitted for chat CLI visibility |
| Chat CLI event polling | `chat.rs` | Unchanged — still reads workflow events from SQLite |
| Signal delivery for non-workflow-bound approvals | `signal.rs` | Direct chat sessions (no workflow) still need signals |

---

## Spec: Simplified approve_request Flow

After rewrite, `approve_request()` becomes:

```rust
pub fn approve_request(config, store, request_id, decided_by, reason) -> Result<ApprovalDecision> {
    // 1. Record decision in SQLite (unchanged)
    let decision = decide_request(config, store, request_id, decided_by, reason, Approved)?;

    // 2. Unblock workflow task (unchanged)
    unblock_task_on_approval(config, store, &decision);
    // → task: AwaitingApproval → Runnable
    // → workflow event: task.approved
    // → join check (if terminal)

    // 3. For non-workflow sessions: send signal (unchanged)
    if should_resume_waiting_session(&decision) {
        resume_session_after_approval(config, store, &decision)?;
    }
    // For workflow-bound tasks: scheduler tick will pick up
    // the Runnable task, re-queue it, and spawn_task_execution
    // will load the TurnContinuation and resume seamlessly.

    Ok(decision)
}
```

The removed parts: `store_approval_result_in_checkpoint()`, `build_approval_resume_message()`, and the complex signal-vs-checkpoint branching.

---

## Spec: Modified spawn_task_execution

```rust
async fn spawn_task_execution(...) {
    // ... existing: load checkpoint, heartbeat, etc. ...

    // NEW: Check for TurnContinuation BEFORE calling spawn_agent_once
    if let Some(continuation) = load_continuation(&cfg, &t_id)? {
        // This is a resume from approval suspension
        let decision = check_approval_decision(&cfg, store, &continuation.approval_request_id)?;
        match decision {
            Some(d) if d.status == Approved => {
                // Execute the approved action
                let result = execute_approved_action(&d, &manifest, ...)?;

                // Reconstruct history from continuation + real result
                let mut history = continuation.history;
                history.push(continuation.assistant_message);
                for (id, name, res) in &continuation.completed_tool_results {
                    history.push(Message::tool_result(id, name, res));
                }
                history.push(Message::tool_result(
                    continuation.pending_tool_call.call_id,
                    continuation.pending_tool_call.tool_name,
                    result,
                ));
                // Execute remaining tool calls if any
                // ... (process continuation.remaining_tool_calls) ...

                delete_continuation(&cfg, &t_id)?;

                // Resume the turn — LLM sees tool result and continues
                let runtime = build_agent_runtime(...)?;
                let reply = runtime.execute_with_history(&mut history).await?;

                // Normal completion path (same as today)
                update_task_run_status(Succeeded, reply_summary);
                dequeue_task();
            }
            Some(d) if d.status == Rejected => {
                // Inject rejection as tool error, let agent handle it
                // ... similar reconstruction but with error result ...
            }
            _ => {
                // Approval still pending — shouldn't happen, re-suspend
                return;
            }
        }
    } else {
        // Normal execution path (no continuation) — existing code, unchanged
        let result = exec.spawn_agent_once(...).await;
        // ... existing Ok/Err handling ...

        // NEW: If result indicates approval suspension,
        // the turn loop already saved the continuation.
        // Just set status and release.
        match result {
            Ok(SpawnOutcome::Completed(reply)) => { /* existing success path */ }
            Ok(SpawnOutcome::Suspended { request_id }) => {
                // Turn loop saved continuation to disk.
                // Task status already set to AwaitingApproval by turn loop.
                dequeue_task();
                release_task_claim();
                return;
            }
            Err(e) => { /* existing failure path */ }
        }
    }
}
```

---

## Spec: Modified execute_with_history Return Type

Currently returns `anyhow::Result<Option<String>>` (the assistant reply text).

Change to return a richer type:

```rust
pub enum TurnOutcome {
    /// Turn completed normally. Contains the final assistant reply text.
    Completed(Option<String>),
    /// Turn suspended at approval boundary. Continuation saved to disk.
    /// Caller should set task to AwaitingApproval and release resources.
    Suspended {
        approval_request_id: String,
    },
}
```

This propagates cleanly to `spawn_agent_once` → `spawn_task_execution` without hacks.

---

## Spec: Modified Turn Loop (lifecycle.rs)

The key change is in the `StopReason::ToolUse` branch (lines 522-599):

```rust
StopReason::ToolUse => {
    let mut assistant_msg = Message::assistant(response.text.clone());
    assistant_msg.tool_calls = response.tool_calls.clone();
    // DO NOT push to history yet — we may need to save it separately

    // ... budget checks ...

    let (had_any_success, results) = processor
        .process_tool_calls(&response.tool_calls, ...)
        .await?;

    // Check if any result requires approval
    let approval_idx = results.iter().position(|(_, _, res)|
        tool_result_requires_approval(res)
    );

    if let Some(idx) = approval_idx {
        let (completed, pending_and_rest) = results.split_at(idx);
        let pending = &pending_and_rest[0];
        let remaining_calls = if idx + 1 < response.tool_calls.len() {
            response.tool_calls[idx + 1..].to_vec()
        } else {
            vec![]
        };

        let request_id = extract_request_id(&pending.2);

        let continuation = TurnContinuation {
            history: history.clone(),
            assistant_message: assistant_msg,
            completed_tool_results: completed.to_vec(),
            pending_tool_call: PendingApprovalToolCall {
                call_id: pending.0.clone(),
                tool_name: pending.1.clone(),
                arguments: find_arguments(&response.tool_calls, &pending.0),
                approval_response: pending.2.clone(),
            },
            remaining_tool_calls: remaining_calls,
            approval_request_id: request_id.clone(),
            workflow_id: self.workflow_id.clone(),
            task_id: self.task_id.clone(),
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            suspended_at: chrono::Utc::now().to_rfc3339(),
            loop_guard_state: self.guard.snapshot(),
        };

        save_continuation(self.config.as_ref().unwrap(), &self.task_id, &continuation)?;

        // Persist history at suspension point (for debugging/audit)
        if let Some(gateway_dir) = self.gateway_dir.as_ref() {
            let _ = persist_history_to_content_store(...);
        }

        return Ok(TurnOutcome::Suspended { approval_request_id: request_id });
    }

    // No approval needed — normal path (push everything to history)
    history.push(assistant_msg);
    for (id, name, result) in results {
        history.push(Message::tool_result(id, name, result));
        // ... existing guard logic ...
    }

    if had_any_success {
        self.guard.register_progress();
    }
}
```

---

## Spec: AwaitingApproval Status Semantics Change

`AwaitingApproval` becomes purely informational for the planner (via `workflow.get_status`), not a state that triggers complex re-queue logic.

**Status meaning (before):** "Turn was killed, agent needs to be re-invoked with a resume message."
**Status meaning (after):** "Turn is suspended, continuation file exists, waiting for approval resolution."

Transitions remain the same:
```
Running → AwaitingApproval   (set by spawn_task_execution when TurnOutcome::Suspended)
AwaitingApproval → Runnable  (set by unblock_task_on_approval, unchanged)
Runnable → Running           (set by process_queued_workflow_tasks, unchanged)
Running → Succeeded/Failed   (set by spawn_task_execution on turn completion)
```

The `Runnable` state is still useful as a signal that the scheduler should re-queue the task. The difference is that on re-queue, `spawn_task_execution` loads the continuation instead of starting a fresh turn.

---

## Spec: Non-Workflow Approval (Direct Chat)

For direct chat sessions (no workflow, no task_id), the current `event.ingest` signal delivery still applies. The continuation model only affects workflow-bound tasks.

For direct chat, the existing behavior is acceptable: the signal arrives, the chat TUI resumes the session, and the agent retries. This path is simpler (no parallel tasks, no joins, no planner) and the LLM usually handles the retry correctly in a single-agent chat.

Future optimization: apply the continuation model to direct chat too, but this is lower priority.

---

## Spec: BlockedApproval Workflow Status Fix

Current bug: `BlockedApproval` is set when any task enters `AwaitingApproval` but never cleared unless join is satisfied.

Fix in `unblock_task_on_approval()`: after transitioning the task to `Runnable`, check if any OTHER tasks are still `AwaitingApproval`. If none, transition workflow back to `WaitingChildren`.

```rust
fn unblock_task_on_approval(config, store, decision) {
    // ... existing: update task status to Runnable/Failed ...

    // NEW: Check if workflow should leave BlockedApproval
    if let Some(wf_id) = &decision.workflow_id {
        let tasks = list_task_runs_for_workflow(config, store, wf_id)?;
        let any_still_awaiting = tasks.iter().any(|t|
            t.status == TaskRunStatus::AwaitingApproval
        );
        if !any_still_awaiting {
            let mut wf = load_workflow_run(config, store, wf_id)?;
            if wf.status == WorkflowRunStatus::BlockedApproval {
                wf.status = WorkflowRunStatus::WaitingChildren;
                save_workflow_run(config, store, &wf)?;
            }
        }
    }
}
```

---

## Spec: Dead Code Removal

Remove these unused fields from `WorkflowRun` in `autonoetic-types/src/workflow.rs`:

- `pending_approval_ids: Vec<String>` — never populated
- `blocked_task_ids: Vec<String>` — never populated

Remove string-based event type detection in `update_task_run_status()` and pass the event type explicitly.

---

## Tasks

### Phase 1: Foundation (no behavior change yet)

- [x] **1.1** Create `autonoetic-gateway/src/runtime/continuation.rs` with `TurnContinuation` type, `save_continuation`, `load_continuation`, `delete_continuation`, `list_suspended_task_ids`. Add `mod continuation;` to `runtime/mod.rs`.
- [x] **1.2** Add `execute_approved_action()` function to `continuation.rs`. Wire to existing sandbox execution (`sandbox_driver::execute_command`) and agent install code paths. Write unit tests for both `SandboxExec` and `AgentInstall` dispatch.
- [x] **1.3** Add `TurnOutcome` enum to `lifecycle.rs` (or a shared types location). Keep `execute_with_history` returning `Result<Option<String>>` for now — `TurnOutcome` will be adopted in Phase 2.
- [x] **1.4** Add `approval_timeout_secs` config field to `GatewayConfig` with default 600. Add `check_approval_timeouts()` function to scheduler (calls `list_suspended_task_ids`, checks elapsed time, marks timed-out tasks as Failed).
- [x] **1.5** Add `workflow.cancel_task` tool to `tools.rs`. Only supports cancelling `AwaitingApproval` and `Pending` tasks. Deletes continuation file, updates task status to `Cancelled`.

### Phase 2: Turn Loop Suspension

- [x] **2.1** Change `execute_with_history` return type from `Result<Option<String>>` to `Result<TurnOutcome>`. Update all callers (`execute_loop`, `spawn_agent_once`) to handle `TurnOutcome::Suspended`.
- [x] **2.2** Modify the `StopReason::ToolUse` branch in `lifecycle.rs` (lines 522-599): detect `approval_required` in tool results → save `TurnContinuation` → return `TurnOutcome::Suspended`. Remove the `approval_required_force_end_turn` flag and the "CRITICAL: end your turn" system message injection.
- [x] **2.3** Modify `spawn_task_execution` in `scheduler.rs` to handle `TurnOutcome::Suspended`: set task status to `AwaitingApproval`, dequeue, release claim. No signal delivery for workflow-bound tasks.
- [x] **2.4** Add `LoopGuard::snapshot()` and `LoopGuard::restore(state)` methods so the guard state survives across suspension/resume.

### Phase 3: Continuation Resume

- [x] **3.1** Modify `spawn_task_execution` in `scheduler.rs`: before calling `spawn_agent_once`, check for `TurnContinuation`. If found + approval resolved → call `execute_approved_action` → reconstruct history → call `execute_with_history` with reconstructed history. Handle remaining tool calls.
- [x] **3.2** Modify `process_runnable_workflow_tasks` in `scheduler.rs`: remove resume message extraction logic (lines 601-618) and stale queued task reconciliation (lines 620-654). The continuation model handles this.
- [x] **3.3** Wire `check_approval_timeouts()` into `run_scheduler_tick_at()` after processing background agents.
- [x] **3.4** Write integration test: agent calls `sandbox.exec` with remote access → task suspends → approve → task resumes → agent gets real exec output → agent continues and completes. Verify the LLM conversation history contains the real tool result, not an approval notification.

### Phase 4: Cleanup

- [x] **4.1** Remove `build_approval_resume_message()` from `approval.rs` (lines 346-423).
- [x] **4.2** Remove `store_approval_result_in_checkpoint()` from `approval.rs` (lines 425-472).
- [x] **4.3** Simplify `approve_request()` in `approval.rs`: remove the `should_resume_waiting_session` branching for workflow-bound tasks. Keep signal delivery only for non-workflow sessions.
- [x] **4.4** Remove `inject_resumption_context()`, `is_resumption` flag, and related logic from `tool_call_processor.rs`.
- [x] **4.5** Remove `pending_approval_ids` and `blocked_task_ids` dead fields from `WorkflowRun` in `workflow.rs`.
- [x] **4.6** Fix `BlockedApproval` clearing in `unblock_task_on_approval()`: check remaining AwaitingApproval tasks and revert to `WaitingChildren` if none.
- [x] **4.7** Replace string-based event type detection in `update_task_run_status()` with explicit parameter.

### Phase 5: Testing and Hardening

Goal: prove the continuation model is correct under concurrency, timeout, cancellation, and recovery scenarios before we declare the rewrite complete.

Current coverage snapshot:
- `turn_continuation_approval_integration::test_approval_continuation_suspends_and_resumes` covers the core suspend/approve/resume path and verifies the resumed LLM sees the real tool result.
- `turn_continuation_approval_integration::test_approval_continuation_file_deleted_on_cancellation` covers continuation file cleanup via direct continuation deletion (low-level behavior).
- `turn_continuation_approval_integration::test_parallel_join_waits_for_approval_task_completion` covers parallel join gating: one task completes while another awaits approval, `workflow.wait` reports `join_satisfied=false`, then flips to `true` only after the approved task completes.
- `turn_continuation_approval_integration::test_workflow_cancel_task_cancels_suspended_task_and_satisfies_join` covers end-to-end cancellation via `workflow.cancel_task`: suspended task cancellation, continuation deletion, terminal task state, and join satisfaction.
- `turn_continuation_approval_integration::test_approval_timeout_fails_task_and_satisfies_join` covers approval timeout: stale `AwaitingApproval` task fails with reason `"Approval timed out"`, continuation is deleted, and join becomes satisfiable.
- `turn_continuation_approval_integration::test_restart_during_suspension_then_approve_and_resume` covers gateway restart behavior: continuation persists across a new `GatewayExecutionService`/store instance, then resumes correctly after approval.
- `turn_continuation_approval_integration::test_two_approval_tasks_both_resume_before_join_satisfies` covers dual approval suspension/resume: both tasks suspend, both are approved, and join is satisfied only after both resumed tasks complete.
- `autonoetic/src/cli/chat.rs` unit tests (`test_format_workflow_event_card_awaiting_approval`, `test_format_workflow_event_card_task_approved`, `test_format_workflow_event_card_task_rejected`) verify chat CLI rendering for `task.awaiting_approval`, `task.approved`, and `task.rejected`.

Hardening exit criteria for this phase:
- Each scenario below is backed by an integration test, not only unit tests.
- Assertions validate both task state transitions and planner-visible workflow status.
- Failure paths assert reason strings exactly (for deterministic operator/debug UX).
- No test relies on synthetic resume messages; all resume behavior flows through persisted continuation state.

- [x] **5.1** Integration test: parallel tasks where one hits approval, others complete. Verify join is not satisfied until approved task also completes. Verify planner sees correct status via `workflow.wait`.
- [x] **5.2** Integration test: approval timeout. Task suspends → no approval for `approval_timeout_secs` → task marked Failed → join fires → planner sees failure reason "Approval timed out".
- [x] **5.3** Integration test: task cancellation via `workflow.cancel_task`. Suspended task cancelled → continuation deleted → task terminal → join fires.
- [x] **5.4** Integration test: gateway restart during suspension. Stop gateway while task is AwaitingApproval → restart → approve → task resumes from continuation → completes.
- [x] **5.5** Integration test: two tasks both hit approval simultaneously. Both approved. Both resume. Join satisfied. Planner resumes.
- [x] **5.6** Verify chat CLI still displays `task.awaiting_approval`, `task.approved`, `task.rejected` events correctly (no changes expected — events still emitted by `update_task_run_status`).

### Phase 6: Documentation

- [x] **6.1** Update `docs/workflow-orchestration.md` with the continuation model, removing the "agent retry with approval_ref" description.
- [x] **6.2** Update `docs/approval-notification-delivery.md` to reflect that workflow-bound approvals no longer use signal delivery.
- [x] **6.3** Update `docs/separation-of-powers.md` to clarify that the gateway executes approved actions directly (within the separation-of-powers model: the agent proposed the action, the operator approved it, the gateway executes it).
- [x] **6.4** Update `CLAUDE.md` with new architecture notes.

---

## Risk Assessment

| Risk | Mitigation |
|---|---|
| `execute_approved_action` must produce identical results to what the tool handler would have produced | Reuse the same sandbox driver and install code paths; don't reimplement |
| Continuation files could accumulate if tasks are abandoned | `check_approval_timeouts` cleans up; also add periodic sweep for orphaned continuations |
| History serialization/deserialization could lose message structure | Use the same `Message` serde that content store persistence already uses |
| Non-workflow approval path (direct chat) unchanged | This is intentional; direct chat is simpler and the current model works acceptably there |
| `remaining_tool_calls` re-execution on resume could fail | Execute them normally through the tool processor; if they fail, the agent sees the failure and handles it |

---

## File Impact Summary

| File | Change Type | Estimated |
|---|---|---|
| `autonoetic-gateway/src/runtime/continuation.rs` | **New** | ~200 lines |
| `autonoetic-gateway/src/runtime/lifecycle.rs` | Modify | ~60 lines changed, ~20 removed |
| `autonoetic-gateway/src/runtime/tool_call_processor.rs` | Remove | ~40 lines removed |
| `autonoetic-gateway/src/runtime/mod.rs` | Modify | +1 line (mod declaration) |
| `autonoetic-gateway/src/scheduler.rs` | Modify | ~80 lines changed, ~50 removed |
| `autonoetic-gateway/src/scheduler/approval.rs` | Simplify | ~300 lines removed |
| `autonoetic-gateway/src/scheduler/workflow_store.rs` | Minor fix | ~15 lines changed |
| `autonoetic-types/src/workflow.rs` | Minor cleanup | ~5 lines removed |
| `autonoetic-types/src/config.rs` | Add field | ~3 lines |
| `autonoetic-gateway/src/runtime/tools.rs` | Add tool | ~60 lines (workflow.cancel_task) |
| `autonoetic-gateway/src/execution.rs` | Modify | ~15 lines (SpawnResult/TurnOutcome) |
| **Net** | | **~+200 new, ~-400 removed** |
