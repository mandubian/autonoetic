# Approval Resolution Delivery

This document describes how approval outcomes are delivered after an operator runs
`autonoetic gateway approve` or `autonoetic gateway reject`.

## Summary

Approval resolution is unified:

1. Decision is always persisted in SQLite (`approvals` table).
2. All post-decision side-effects fan out through `apply_decision`.
3. Workflow-bound tasks resume from the signed `SessionCheckpoint` stored when the turn suspended.
4. Non-workflow sessions still use durable notification delivery.

This keeps workflow orchestration deterministic while preserving direct-chat compatibility.

## Two Delivery Paths

### 1) Workflow-Bound Tasks (checkpoint resume model)

If an approval request has both `workflow_id` and `task_id`:

- Decision is recorded in `approvals`.
- `apply_decision` updates the workflow task status (`Runnable` on approve, `Failed` on reject).
- Scheduler picks runnable tasks and re-executes them.
- Execution loads the `SessionCheckpoint`, verifies checkpoint/approval action-equality, injects `approval_ref` into the suspended tool call, resumes the reasoning loop, and lets the agent re-issue the tool call with `approval_ref`. The gateway executes the tool normally and injects the real result.
- No `approval_resolved` signal is required for the child-task checkpoint-resume path.
- Parent workflow visibility comes from workflow events plus `ChildStateNotification` delivery on child state transitions, not from a separate approval notification row.

### 2) Non-Workflow Sessions (notification model)

If the request is not workflow-bound:

- Decision is recorded in `approvals`.
- `apply_decision` writes a durable approval signal to `notifications`.
- Gateway-owned consumers/channel clients deliver and acknowledge the signal.
- This path preserves existing direct-chat continuation behavior.

## Storage Model

All approval state is stored in `runtime/gateway.db`:

- `approvals`: request metadata + decision status (`pending`/`approved`/`rejected`)
- `notifications`: durable queued notifications for non-workflow delivery
- `workflow_events`: workflow-visible state transitions (`task.awaiting_approval`, `task.approved`, `task.rejected`, etc.)

## Operator Expectations

- Approve/reject is always durable and auditable.
- Workflow tasks resume from checkpoint without requiring manual retry prompts.
- Non-workflow chat sessions still receive durable approval notifications.

## Notes

- Workflow chat visibility should be read from workflow events, not notification payloads.
- Approval records remain queryable regardless of which delivery path is used.
- The single `apply_decision` path is also used for cancel, withdraw, and cancel-for-task.

## Related Docs

- `docs/internals/workflow-orchestration.md`
- `docs/concepts/separation-of-powers.md`
