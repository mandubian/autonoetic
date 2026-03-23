# Workflow Orchestration

The gateway owns a durable workflow orchestration layer that coordinates multi-agent execution across approval boundaries, planner restarts, and gateway restarts.

This layer sits above the session/timeline infrastructure and provides explicit orchestration metadata instead of relying on session-path string parsing.

## Core Concepts

### WorkflowRun

One durable workflow per user-facing task. Created automatically when the first `agent.spawn` occurs for a root session.

```
workflow_id       Unique identifier (e.g. wf-a1b2c3d4)
root_session_id   The lead planner's session
lead_agent_id     The planner agent (e.g. planner.default)
status            Active | WaitingChildren | BlockedApproval | Resumable | Completed | Failed | Cancelled
created_at        RFC3339 timestamp
updated_at        RFC3339 timestamp
join_task_ids     Task IDs the planner is waiting on
join_policy       AllOf | AnyOf | FirstSuccess | Manual
```

### TaskRun

Each delegated child execution becomes a durable task record. Survives gateway restarts and approval boundaries.

```
task_id           Unique identifier
workflow_id       Parent workflow
agent_id          Target specialist (e.g. coder.default)
session_id        Child delegation session (e.g. root/coder-abc123)
parent_session_id Delegating agent's session
status            Pending | Runnable | Running | AwaitingApproval | Paused | Succeeded | Failed | Cancelled
created_at        RFC3339 timestamp
updated_at        RFC3339 timestamp
source_agent_id   Which agent spawned this task
result_summary    Short completion summary (not full transcript)
join_group        Optional group for collective join conditions
message           Original kickoff message (preserved across approval boundaries)
metadata          Original delegation metadata (preserved across approval boundaries)
```

### QueuedTaskRun

A pending task awaiting scheduler pickup. Persisted to disk so it survives gateway restarts.

```
task_id           Unique identifier
workflow_id       Parent workflow
agent_id          Target specialist
message           Kickoff message for the child agent
child_session_id  Delegation path used as child session_id
parent_session_id Source (parent) session
source_agent_id   Agent that initiated the spawn
metadata          Optional metadata passed through to child
join_group        Optional join group
blocks_planner    Whether this task blocks planner continuation
enqueued_at       RFC3339 timestamp
```

### ApprovalBarrier

Approvals bind to tasks and workflows, not inferred session names.

```
approval_id       Unique identifier
workflow_id       Parent workflow
task_id           Blocked task (optional)
kind              Type of approval
status            Pending | Approved | Rejected
subject           Human-readable description
requested_at      RFC3339 timestamp
resolved_at       RFC3339 timestamp (when resolved)
resume_target     Task(s) to unblock on resolution
```

## Execution Semantics

### Async Spawn

`agent.spawn` creates a `QueuedTaskRun` and returns immediately with `task_id`. The gateway scheduler picks up queued tasks on its next tick and spawns background tokio tasks.

```rust
// Planner code (conceptual)
let result = agent.spawn(
    agent_id: "coder.default",
    message: "Build a REST API with authentication",
    metadata: { "priority": "high" },
    async: true,  // Returns immediately
);
// result: { task_id: "task-xyz", session_id: "root/coder-abc", accepted: true }
```

### Task Lifecycle

```
Pending → Runnable → Running → Succeeded
                       ↓
                 AwaitingApproval → Runnable → Running → Succeeded
                       ↓
                    Failed (on reject)
```

### Crash Recovery

The scheduler maintains durability across process crashes:

1. **Queue file** (`QueuedTaskRun`) survives gateway restarts
2. **TaskRun status** (`Running`) indicates task is in progress
3. **On tick**: If `TaskRun` is `Running` and queue file exists, the task is already being executed — skip (don't re-spawn)
4. **On terminal completion**: The spawned task removes the queue file and updates `TaskRun` to `Succeeded`/`Failed`

### Message/Metadata Preservation

When a task enters `AwaitingApproval`, its `message` and `metadata` are preserved in the `TaskRun`. When approval is granted and the task resumes, the original kickoff message is replayed — not a synthetic "Resume after approval" message.

## Planner Interface

Planners interact with the orchestration layer through:

- **`agent.spawn(..., async: true)`** — Enqueue a child task and return immediately
- **`workflow.wait({ task_ids, policy })`** — Check task statuses and join condition
- **`workflow.get_status({ workflow_id })`** — Get workflow state
- **`workflow.get_results({ task_ids })`** — Get completed task outputs

### Planner Lifecycle

The planner is not "done" when it delegates:

1. Planner turn executes
2. Gateway checkpoints planner state
3. Planner enters `WaitingChildren`
4. Child tasks execute independently
5. On approval: blocked tasks pause, others continue
6. On approval resolution: blocked tasks resume
7. When join condition is satisfied: planner becomes `Resumable`
8. Gateway resumes planner with child outputs

## Join Policies

- **`AllOf`**: All tasks must complete (default)
- **`AnyOf`**: Any task completion satisfies
- **`FirstSuccess`**: First success satisfies; failures ignored
- **`Manual`**: Explicit `workflow.wait` call required

## CLI Integration

### Trace Commands

```bash
# Follow a workflow's event stream
autonoetic trace workflow <workflow_id> --follow

# Follow by root session
autonoetic trace workflow --root <session_id> --follow

# Graph view
autonoetic trace graph <workflow_id> --follow
```

### Chat Integration

The chat pane shows a compact workflow summary at turn end:

```
[workflow] wf-a1b2c3 · 2 running · 1 done · 1 blocked on approval
```

## Workflow Store

All orchestration state lives under `.gateway/scheduler/workflows/`:

```
workflows/
└── runs/
    └── <workflow_id>/
        ├── workflow.json          # WorkflowRun
        ├── tasks/
        │   └── <task_id>.json    # TaskRun
        └── checkpoints/
            ├── planner.json      # WorkflowCheckpoint (latest)
            └── tasks/
                └── <task_id>.json  # TaskCheckpoint (latest)
```

**Note:** `WorkflowEventRecord` streams are stored in the Gateway's embedded SQLite database (`.gateway/gateway.db`) rather than as `events.jsonl` file appends, ensuring high concurrency and reliability.

## Workflow Event Types

The workflow system emits structured events for all state transitions. These events are consumed by the chat CLI to display real-time progress and by other tools for monitoring.

### Workflow-Level Events

| Event Type | Description |
|-----------|-------------|
| `workflow.started` | A new workflow has been created for a root session |
| `workflow.join.satisfied` | All tasks in a join group have completed |

### Task Lifecycle Events

| Event Type | Description | Payload |
|-----------|-------------|---------|
| `task.spawned` | A task has been created and queued for execution | `{ task_id, agent_id, message }` |
| `task.started` | A task has started executing (agent spawned) | `{ status: "Running" }` |
| `task.completed` | A task completed successfully | `{ status: "Succeeded" }` |
| `task.failed` | A task failed | `{ status: "Failed" }` |
| `task.updated` | Generic task status update (catch-all) | `{ status: ... }` |
| `task.awaiting_approval` | Task requires approval before proceeding | `{ status: "AwaitingApproval" }` |

### Approval Events (New)

| Event Type | Description | Payload |
|-----------|-------------|---------|
| `task.awaiting_approval` | Task is waiting for approval approval | `{ status: "AwaitingApproval", approval: "sandbox_exec\|agent_install" }` |
| `task.approved` | Approval was granted, task is now runnable | `{ status: "Runnable", approval: "approval_approved" }` |
| `task.rejected` | Approval was rejected, task has failed | `{ status: "Failed", approval: "approval_rejected" }` |

### Chat CLI Display

The chat CLI polls workflow events and displays them with appropriate icons:

```
📋 [2026-03-23T13:41:00] Workflow started
🚀 [2026-03-23T13:41:05] Task spawned: task-94c19ac6
▶ [2026-03-23T13:41:10] Task started: task-94c19ac6
⏸ [2026-03-23T13:41:15] Approval required: task-94c19ac6 (sandbox.exec)
✅ [2026-03-23T13:42:30] Approval approved: task-94c19ac6
🔁 [2026-03-23T13:42:35] Task resumed: task-94c19ac6
✅ [2026-03-23T13:43:00] Task completed: task-94c19ac6
✅ [2026-03-23T13:43:05] Workflow join satisfied
```

### Approval Flow via Workflow Events

Approval tracking is now unified through workflow events only:

1. **Tool requires approval** → Task status set to `AwaitingApproval` → `task.awaiting_approval` event emitted
2. **Chat CLI polls events** → Shows "⏸ Approval required: task-xxx (sandbox.exec)"
3. **User approves via CLI** → `autonoetic gateway approve apr-xxx`
4. **Task status updated** → Status changed to `Runnable` → `task.approved` event emitted
5. **Chat CLI polls events** → Shows "✅ Approval approved: task-xxx"
6. **Agent wakes up** → Receives `approval_resolved` signal → Retries tool with `approval_ref`
7. **Task executes** → Returns to `Running` → Eventually completes

This unified approach means:
- **No SQLite approval polling** in chat CLI
- **Single source of truth**: workflow events
- **Consistent visibility**: All state transitions visible through events
- **Approvals persist** in SQLite for the approval command, but chat uses events for display

## Causal Chain Relationship

Workflow orchestration and causal chain are separate layers:

- **Causal chain**: Immutable audit log of what happened (gateway actions, agent actions, approvals, tool results)
- **Workflow store**: Mutable operational state of what should happen next
- **CLI/graph**: Human-facing projections built from workflow events + causal chain

Important: Every significant workflow transition emits a causal chain entry (`workflow.started`, `workflow.task.spawned`, `workflow.task.completed`, etc.) so audit and orchestration remain correlatable.

## Bug Fixes

### Bug 1: Queued tasks re-spawned on every scheduler tick

**Problem**: Crash recovery treated `Running` tasks as needing re-spawn, but the queue file was only removed on terminal completion. Tasks running longer than one tick were re-spawned on every tick.

**Fix**: If a `TaskRun` is already `Running` when the scheduler tick processes it, the scheduler skips it entirely (doesn't re-spawn). The queue file is removed only when the task actually completes. Crash recovery works because the queue file persists until completion.

### Bug 2: Approval-resumed tasks lost original inputs

**Problem**: When resuming after approval, the gateway sent a synthetic "Resume after approval: <session_id>" message and `None` metadata, discarding the original kickoff message.

**Fix**: `TaskRun` now stores `message` and `metadata` fields. These are populated on spawn and preserved through the `AwaitingApproval` → `Runnable` transition. Resume uses the original values.

### Bug 3: Inverted event deduplication logic prevented workflow events from displaying

**Problem**: The chat CLI's event filtering logic had an inverted condition (`!insert()` instead of `insert()`), causing it to skip actually new events and only show duplicates. This prevented workflow events like `task.awaiting_approval`, `task.approved`, etc. from appearing in the chat interface.

**Fix**: Corrected the deduplication logic to use `insert()` directly, which returns `true` for newly inserted events. Also improved workflow change detection to reset the bootstrap flag when a workflow is created mid-session.

### Bug 4: Short alias used as full handle prevented content.read from working

**Problem**: When `content.write` returns an alias (e.g., `"8b40c8e1"`), the LLM sometimes mistakenly treats it as a full SHA-256 handle by prepending `sha256:`, resulting in `content.read("sha256:8b40c8e1")`. The lookup logic treated this as a full handle lookup (expecting 64 hex chars) rather than an alias lookup (8 chars), causing "Content not found" errors.

**Fix**: Enhanced `ContentStore::read_by_name_or_handle` to detect the pattern `sha256:SHORT_ALIAS` (exactly 8 hex chars after the prefix) and redirect it to alias lookup. This makes content.read more resilient to LLM misinterpretations of the alias value.

## Files

- `autonoetic-gateway/src/scheduler.rs` — Scheduler tick, spawn logic, crash recovery
- `autonoetic-gateway/src/scheduler/workflow_store.rs` — Durable store, task/workflow updates, join conditions
- `autonoetic-types/src/workflow.rs` — Core types (`WorkflowRun`, `TaskRun`, `QueuedTaskRun`, etc.)
- `autonoetic-gateway/src/scheduler/workflow_causal.rs` — Causal chain mirroring
- `autonoetic-gateway/src/runtime/content_store.rs` — Content addressing, visibility, alias/handle resolution
