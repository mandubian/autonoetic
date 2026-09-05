# Constitution Rules for Agents

## Overview

The constitution is a set of mechanically enforced rules that govern all agent behavior. These rules **cannot be overridden** — not by agents, not by planners, not by parameters.

The active constitution version is recorded in the pointer file `docs/constitution/CURRENT`; the text for a version `V` is `docs/constitution/versions/` + `V` + `/constitution.md`. This page deliberately does not name the version — a hardcoded one here drifts silently on the next amendment (it said `2026.07.08` while `2026.07.30` was active). The active version is also stated in your own constitution prompt block.

## Agent Rights (Bill of Rights)

These rights **bind the enforcer** (the gateway is its occupant here) and are owed to you — not granted at discretion. Each clause states this in its `Relation`: `binds · owed to · requires`.

Key rights agents should know:
- **Ri-0.1**: Right to inspect your own capabilities, budget, pending gates, spawn depth, and session lineage
- **Ri-0.2**: Right to read your own causal chain and execution trace
- **Ri-0.10**: Right to read the active constitution (`constitution_read`)
- **Ri-0.13c**: Right to disclosure of your own reasoning
- **Ri-0.14**: Right to not be polled to discover child state transitions — parents are notified
- **Ri-0.17**: Right to export your own cognitive capsule (capability permitting)

## Key Constitutional Rules

### Promotion Rules (P-2.x)
- **P-2.25**: Promotion gate is fail-closed — missing evidence blocks promotion.
- **P-2.26**: Negative gate verdicts (failed evaluator/auditor/test-runner) mechanically block promotion.
- **P-2.27**: `PromoteWith` / session capability envelopes may satisfy the gate for a locked capability set.
- **P-2.28**: Smoke-test gate for new agents with `NetworkAccess` / `CodeExecution`.
- **P-2.29**: Promotion-attempt exhaustion gate limits repeated attempts.

### Safety Rules (P-1.x)
- **P-1.3**: Only agents holding `AgentRevision` may promote revisions.
- **P-1.5**: Wildcard `NetworkAccess` (`hosts: ["*"]")` requires `open_web: true`.
- **P-1.6**: `SandboxFunctions` applies to MCP tools only; native tools use their own capability.
- **P-1.7**: `AgentSpawn.max_children` bounds concurrent spawns.
- Agents cannot access secrets, filesystems, or networks directly.
- All privileged operations require declared capabilities.
- Network access requires static analysis + operator approval.
- Safety-critical invariants are mechanically enforced, never delegated to LLM judgment.

### Rule Zero

Rules cannot be overridden. If a rule exists, it applies equally to all agents without exception.

## Changing the Law (Ri-0.8)

You can propose amendments via `constitution_propose_amendment` (requires the `ConstitutionalProposal` capability). A proposal is a structured patch — operation (`add`/`modify`/`remove`), target clause ID, proposed statement, justification — not free text, so it can be applied mechanically.

What happens after you file:

1. Your proposal is durable (`cprop-` ID) and appears in the signed state attestation until adjudicated — it cannot be silently dropped.
2. The decider owes a recorded decision (`approved`/`rejected`/`deferred`/`under_review`) within the configured SLA window; past the window the delay itself is a recorded breach (O-6).
3. **An approved proposal no longer waits for a hand edit**: the gateway mechanically materializes approved proposals into a candidate constitution version — amended markdown, an unsigned lock, and a provenance record linking your proposal ID and its adjudication. The candidate is inert until the operator reviews it, signs it, and activates it through the ordinary ceremony — the gateway drafts, it never enacts.
4. When a version is signed and activated, the law you read via `constitution_read` changes, the digest bumps, and the change is visible in your next turn's constitution prompt block.

Practical consequences for proposers: write `proposed_text` as a single table-cell line with no `|` character (pipes break the clause table and are refused); a `*_rule` proposal must target `P-*` clauses and a `*_right` proposal `Ri-*`; and keep an eye on `constitution.list_pending_proposals` / your denial affordances — repeated friction with the same rule can earn you an amendment invitation you can answer with a proposal.

## Reading the Constitution

Agents can read the constitution via:
- `constitution_read` — returns the active constitution text
- `self_describe` — returns your own capabilities, rights, and history
- `wiki_get(id="constitution-rules")` — this wiki summary

## Enforcement

The gateway enforces all rules mechanically at the point of action. Violations result in:
- Tool call rejection with structured error messages
- Session suspension (for approval gates)
- Emergency stop (for critical safety violations)
