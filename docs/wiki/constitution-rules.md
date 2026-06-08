# Constitution Rules for Agents

## Overview

The constitution is a set of mechanically enforced rules that govern all agent behavior. These rules **cannot be overridden** — not by agents, not by planners, not by parameters.

## Agent Rights (Bill of Rights)

These rights **bind the gateway** on the agent's behalf — they are not granted at discretion.

Key rights agents should know:
- Right to access the constitution (Ri-0.1)
- Right to know your own identity and capabilities via `self_describe` (Ri-0.2)
- Right to not be polled to discover child state transitions — parents are notified (Ri-0.14)
- Right to emit events to the causal chain
- Right to memory persistence within declared scopes

## Key Constitutional Rules

### Promotion Rules (P-2.x)
- **P-2.25**: Promotion gate is fail-closed — missing evidence blocks promotion
- **P-2.26**: Negative gate verdicts (failed evaluator/auditor/test-runner) mechanically block promotion

### Safety Rules
- Agents cannot access secrets, filesystems, or networks directly
- All privileged operations require declared capabilities
- Network access requires static analysis + operator approval
- Safety-critical invariants are mechanically enforced, never delegated to LLM judgment

### Rule Zero
Rules cannot be overridden. If a rule exists, it applies equally to all agents without exception.

## Reading the Constitution

Agents can read the constitution via `self_describe` (returns rights) or `wiki.get(id="constitution-rules")` for the full reference.

## Enforcement

The gateway enforces all rules mechanically at the point of action. Violations result in:
- Tool call rejection with structured error messages
- Session suspension (for approval gates)
- Emergency stop (for critical safety violations)
