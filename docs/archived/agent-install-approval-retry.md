# Archived: Agent Install Approval and Retry

This document was moved to `archived` because it no longer reflects the current gateway behavior.

## Why archived

The previous version described a file-based approval payload workflow and an automatic `agent.install` auto-execute path after approval. Current implementations are SQLite-first and are documented in:

- `docs/approval-system.md`
- `docs/approval-notification-delivery.md`
- `docs/workflow-orchestration.md`

## Replacement guidance

For current operator and developer behavior, treat these as source-of-truth:

1. `docs/approval-system.md` for approval/user.ask/clarification semantics.
2. `docs/workflow-orchestration.md` for workflow task states and resume flow.
3. `docs/approval-notification-delivery.md` for delivery path differences.

Historical content remains available in git history if needed.
