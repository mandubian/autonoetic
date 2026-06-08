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

For revisions declaring high-risk capabilities (`NetworkAccess`, `CodeExecution`, `AgentSpawn`), promotion requires:
- **Evaluator pass** record (sealed evaluation or static evaluation)
- **Auditor pass** record (security, governance, reproducibility check)
- Evidence is validated against the revision's canonical `content_digest`

## Gate Types

| Gate | Required For | Evidence |
|------|-------------|----------|
| **Legacy** | High-risk capabilities | Evaluator + Auditor pass records |
| **FullJury** | Federation scenarios | Federation verdicts + approved operator escalation |

## Mechanical Enforcement (Constitution P-2.26)

The gateway mechanically rejects promotion if any executed gate role returned a negative verdict:
- If `unit_test_runner_id` is present and `unit_test_runner_pass` is `false` → **promotion blocked**
- If evaluator ran and `evaluator_pass` is `false` → **promotion blocked**
- If auditor ran and `auditor_pass` is `false` → **promotion blocked**

Missing gate roles do NOT block promotion (fail-closed for negative verdicts only).

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
