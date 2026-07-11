# Promotion Lifecycle

## Overview

Promotion is the process of activating a new agent revision. It ensures that only validated, reviewed code becomes the active version of an agent.

## Flow

```
1. Agent code is packaged into an artifact (artifact_build)
2. An immutable revision is created from the artifact (agent_revision_create / create_from_intent)
3. Evaluator and auditor review the revision and record evidence (promotion_record)
4. The revision is promoted to active (agent_revision_promote)
```

## Evidence Binding

For revisions declaring high-risk capabilities (`NetworkAccess`, `CodeExecution`, `AgentSpawn`, `ArtifactExecution`), promotion requires one or more of:
- **Static evaluator** pass record (`static_evaluator.default`) — LLM-set, severity-gated
- **Execution evaluator** pass records (`unit_test_runner.default`, `sealed_evaluator.default`) — trace-derived
- **Auditor** pass record (`auditor.default`) — security, governance, reproducibility
- Evidence is validated against the revision's canonical `content_digest`

## `promotion_record` Severity Gating

`promotion_record` mechanically rejects:
- `pass=true` with any `error` or `critical` finding
- `pass=true` with `warning` findings that lack a non-empty `evidence` field

## Gate Types

| Gate | Required For | Evidence |
|------|-------------|----------|
| **Legacy** | High-risk capabilities | Evaluator + Auditor pass records |
| **FullJury** | Federation scenarios | Federation verdicts + approved operator escalation |

## Mechanical Enforcement (Constitution P-2.25 – P-2.29)

- **P-2.25**: Promotion gate is fail-closed — missing required evidence blocks promotion.
- **P-2.26**: Negative gate verdicts mechanically block promotion.
- **P-2.27**: `PromoteWith` / session capability envelopes may satisfy the gate for a locked capability set.
- **P-2.28**: Smoke-test gate for new agents with `NetworkAccess` / `CodeExecution`.
- **P-2.29**: Promotion-attempt exhaustion gate limits repeated attempts.

## Rollback

```
agent_revision_promote → new revision becomes active
agent_revision_rollback → reverts to previous revision
agent_revision_rollback --to <rev-id> → reverts to specific revision
```

## Revision Identity

Content digest determines revision identity — identical content reuses an existing revision. If a later revision resolves to a different `content_digest`, existing promotion evidence is cleared and evaluator/auditor must re-run.

## Federation

In federated deployments, promotion verdicts can be shared across gateway instances. Federation requires both local evidence and cross-gateway attestation.
