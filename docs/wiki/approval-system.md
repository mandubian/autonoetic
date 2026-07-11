# Approval System

## Overview

When an agent requests a privileged action (network access, file system writes, etc.), the gateway may require operator approval before proceeding. The approval system is a multi-layered deduplication pipeline designed to minimize operator burden while maintaining security.

## Approval Deduplication Layers (checked in order)

1. **Exec Cache** (fingerprint-level, cross-session) — only when all patterns are concrete (url_literal/ip_address). If the exact command+targets fingerprint was previously approved, reuse it automatically.

2. **Plan Grants** — when the operator approves a plan, the declared network envelope is materialized as a session approval grant. Subsequent tool calls targeting hosts within the plan's envelope skip straight to execution. Revoked on envelope-expanding amend. Plan grants are a distinct dedup layer that sits above session grants. See [`docs/plan-capability-grants.md`](../plan-capability-grants.md).

3. **Session Approval Grants** (target-level, scope-aware, within root session) — stored in `session_approval_grants` + `session_approval_grant_targets` tables. Supports pattern types: `ExactHost`, `HostSuffix`, `HostAndPort`, `UrlPrefix`. Scoped `RootSession` or `Session`. Optional expiry (`expires_at`).

4. **Existing Approved/Pending Approvals** (domain-level matching) — checks if a similar approval already exists.

5. **Approval Flood Cap** (`max_pending_approvals_per_root`, default 50) — rejects requests that would exceed the cap with `approval_flood`.

## Gate Types

| Gate Type | Description | Resolution |
|-----------|-------------|------------|
| **Approval** | Agent requests a privileged action | Approve or reject |
| **Interaction** | Agent asks the operator a question | Answer with text or choice |
| **Plan** | Agent proposes a plan for review | Approve or reject |
| **Escalation** | Promotion or federation decision requires a decider | Approve or reject (operator or agent-decider with `GateDecider`) |

## Recent Constitutional Hardening (P-2.25 – P-2.29)

- **P-2.25**: Promotion gate is fail-closed — missing evidence blocks promotion.
- **P-2.26**: Negative gate verdicts (failed evaluator/auditor/test-runner) mechanically block promotion.
- **P-2.27**: `PromoteWith` / session capability envelopes may satisfy the promotion gate for a locked capability set.
- **P-2.28**: Smoke-test gate for new agents with `NetworkAccess` / `CodeExecution`.
- **P-2.29**: Promotion-attempt exhaustion gate limits repeated promotion attempts.

## Grant Revocation

Grants can be revoked without triggering an emergency stop:
```
gateway grants revoke --root-session <id> --host X
```
Emits a `grant_revocation` causal event.

## Similarity Scoring

Similarity scoring for sandbox-exec approvals was removed in #565. The score was write-only: nothing consumed it, so the `approval_similarity.rs` module and the `similar_to_request_id` / `similarity_score` columns were deleted. A small Jaccard advisory check for wiki proposals is inlined in `human_gate.rs`.

## Checkpoint Resume

Approval requests at turn boundaries use a signed `SessionCheckpoint`: the agent's turn is suspended, and when the operator resolves the approval, the turn resumes from checkpoint. Checkpoints are HMAC-SHA256 signed with `continuation_key` (or derived from `node_id`) and verify action-equality against the stored approval.

## When Approvals Are Needed

- **Network access**: Detected by static analysis of sandboxed code (imports, function calls)
- **High-risk promotions**: Revisions with `NetworkAccess`, `CodeExecution`, or `AgentSpawn` capabilities
- **Credential operations**: Setup of new credentials may require human-assisted entry
- **Plan approval**: Planner proposes a plan before executing
