# Workflow Orchestration and Child Agents

## Spawning Children

Use `agent_spawn` to create child agent sessions:

```json
{
  "agent_id": "coder.default",
  "message": { "task": "Implement the weather API client" },
  "metadata": {
    "delegated_role": "coder",
    "delegation_reason": "Implement weather API integration",
    "parent_goal": "Create a weather agent"
  }
}
```

Always include structured `metadata` with `delegated_role`, `delegation_reason`, and `parent_goal`.

## Waiting for Children

### Sequential / Single Child
Spawn `async=true`, then **end your turn**. The gateway suspends you as `WaitingForChild` and resumes you when the child completes. **Do not call `workflow_wait`.**

### Parallel Fan-Out
Spawn all children `async=true`, then make **one** `workflow_wait(task_ids=[all], timeout_secs=N)` call. It blocks once and returns when the whole group is terminal.

### Status Snapshot
`workflow_wait(timeout_secs=0)` returns immediately without blocking.

## Key Rules

1. **Never poll** — do not loop on `workflow_wait` or `workflow_state` to discover progress
2. **Yield for sequential** — end your turn, the gateway will resume you
3. **Join for parallel** — one `workflow_wait` call to block until all children complete
4. **Read guards on resume** — `workflow_state` once per wake, never in a loop

## Delegation Ladder (for Planner)

1. **Foundational match**: route directly to the appropriate foundational agent
2. **Unknown intent**: `discovery.default` → semantic match among installed agents
3. **No candidate**: `agent-factory.default` → builds new agent end-to-end
4. **Recurring task**: `agent-factory.default` → install agent → `scheduler_cron_create`

## Workflow Signals

Workflows support signal-based coordination:
- `workflow.child_state` — emitted when a child transitions state
- `workflow.join_satisfied` — emitted when all children in a join group complete
- `workflow.signal` — custom signals for cross-agent coordination

## Task Cancellation

Use `workflow_cancel_task(task_id)` to cancel a running child. The gateway:
- Sends a cancellation signal to the child
- Waits for graceful shutdown
- Cleans up all associated resources
- Emits a `task.cancelled` event to the causal chain
