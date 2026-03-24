# Approval Resolution Delivery

This document describes how approval outcomes are delivered after an operator runs
`autonoetic gateway approve` or `autonoetic gateway reject`.

## Summary

Approval resolution is now **path-dependent**:

1. Decision is always persisted in SQLite (`approvals` table).
2. Workflow-bound requests resume via **turn continuation**, not session notification.
3. Non-workflow requests still use durable notification delivery.

This keeps workflow orchestration deterministic while preserving direct-chat compatibility.

## Two Delivery Paths

### 1) Workflow-Bound Tasks (continuation model)

If an approval request has both `workflow_id` and `task_id`:

- Decision is recorded in `approvals`.
- Workflow task is unblocked (`Runnable` on approve, `Failed` on reject).
- Scheduler picks runnable tasks and re-executes them.
- Execution loads `TurnContinuation`, executes approved action in the gateway, injects real tool result, and continues the turn.
- No `approval_resolved` signal is required for this path.

### 2) Non-Workflow Sessions (notification model)

If the request is not workflow-bound:

- Decision is recorded in `approvals`.
- A durable approval signal is written to `notifications`.
- Gateway-owned consumers/channel clients deliver and acknowledge the signal.
- This path preserves existing direct-chat continuation behavior.

## Storage Model

All approval state is stored in `.gateway/gateway.db`:

- `approvals`: request metadata + decision status (`pending`/`approved`/`rejected`)
- `notifications`: durable queued notifications for non-workflow delivery
- `workflow_events`: workflow-visible state transitions (`task.awaiting_approval`, `task.approved`, `task.rejected`, etc.)

## Operator Expectations

- Approve/reject is always durable and auditable.
- Workflow tasks resume from continuation without requiring manual retry prompts.
- Non-workflow chat sessions still receive durable approval notifications.

## Notes

- Workflow chat visibility should be read from workflow events, not notification payloads.
- Approval records remain queryable regardless of which delivery path is used.

## Related Docs

- `docs/workflow-orchestration.md`
- `docs/separation-of-powers.md`
