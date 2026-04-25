# Interaction Mechanisms: Approval vs user_ask vs clarification_needed

## Overview

Autonoetic has three distinct mechanisms for human/agent interaction during workflow execution. Each serves a different purpose and has different semantics. Confusing them leads to deadlocks, stranded sessions, and infinite retry loops.

## Comparison Table

| Property | Approval | user_ask | clarification_needed |
|---|---|---|---|
| **Purpose** | Gate privileged operations | Ask human questions | Child requests info from parent |
| **Triggered by** | Tool (sandbox_exec, agent.install) | Agent explicitly calls tool | Child agent return value |
| **Who resolves** | Operator (CLI) | Human (chat/CLI) | Parent agent |
| **Session state** | `YieldReason::ApprovalRequired` | `YieldReason::UserInputRequired` | Normal tool result |
| **Resume mechanism** | Auto-resume by Gateway | Gateway-owned `interaction.answer` / checkpoint resume (`resume_from_user_interaction`) | Parent re-spawns child |
| **Outcome type** | `TurnOutcome::Suspended` | `TurnOutcome::SuspendedUserInput` | `TurnOutcome::Completed` |
| **Close reason** | `jsonrpc_spawn_suspended_approval` | `jsonrpc_spawn_suspended_user_input` | `jsonrpc_spawn_complete` |
| **Available during orchestration** | ✅ Yes (primary mechanism) | ❌ Blocked | ✅ Yes (structured result) |
| **Persisted to** | SQLite (`approvals` table) | SQLite (`user_interactions` table) | Conversation history |
| **Deterministic** | Yes (stored payload replay) | No (human answers freely) | Yes (structured signal) |

## Approval

### What It Is

A **Gateway-enforced security gate** for privileged operations. Before any operation that could affect external systems (network access, agent installation, file writes), the Gateway suspends execution and requires an operator to explicitly approve.

### When To Use

- `sandbox_exec` with network access (HTTP requests, socket connections)
- `agent.install` for evolution roles
- Any operation flagged by security analysis as high-risk

### When NOT To Use

- Asking the user for clarification on their intent
- Getting user preferences or choices
- Coordinating between parent and child agents

### How It Works

```
Agent → Tool detects approval needed
     → Creates ApprovalRequest (persisted to SQLite)
     → Returns {"ok": false, "approval_required": true, "request_id": "apr-xxx"}
     → Session suspends (checkpoint saved)

Operator → gateway approvals approve apr-xxx

Gateway → Loads ApprovalRequest
        → Resumes suspended session via signal delivery
        → Executes approved action with real results
        → Injects results into agent conversation

Agent → Continues with real tool output
```

### Key Properties

- **Auto-resume**: Gateway automatically resumes the session when approval is resolved
- **Payload preservation**: For `agent.install`, the full args are stored and replayed on retry (no payload drift)
- **Domain-level reuse**: For `sandbox_exec`, once a domain is approved at the root workflow level, other sessions can reuse it
- **Continuation-based**: The approved command is stored in a continuation file and replayed with real results

### Example

```json
// Tool response when approval needed
{
  "ok": false,
  "approval_required": true,
  "request_id": "apr-229204ca",
  "message": "Install requires approval. To proceed: 1) Get the request approved by an operator, 2) Retry agent.install with the EXACT same payload PLUS add promotion_gate.install_approval_ref = 'apr-229204ca' to your JSON.",
  "approval": {
    "kind": "agent_install",
    "summary": "weather_lookup with NetworkAccess",
    "detected_network_hosts": ["api.open-meteo.com", "geocoding-api.open-meteo.com"],
    "retry_field": "promotion_gate.install_approval_ref"
  }
}
```

## user_ask

### What It Is

A **UI primitive** that lets an agent ask a human a question and suspend execution until answered. Creates a `UserInteraction` record and checkpoints the session.

### When To Use

- Asking the user for clarification on ambiguous requirements
- Getting user preferences (e.g., "Which output format do you prefer?")
- Presenting choices for the user to select from

### When NOT To Use

- **NEVER for approval handling** — use `workflow_wait` instead
- During active workflow orchestration (blocked by runtime guard)
- When child tasks are pending or approvals are outstanding

### How It Works

```
Agent → user_ask({"question": "What format do you want?"})
     → Creates UserInteraction (persisted to SQLite)
     → Returns TurnOutcome::SuspendedUserInput
     → Session suspends (checkpoint saved)

Human → Answers via chat/CLI or messenger adapter

Gateway → `interaction.answer` / `interaction.resolve_and_answer` (or in-process orchestration)
        → Persists answer + resumes: workflow tasks `Paused`→`Runnable` + queue, or
          `resume_from_user_interaction` for non-workflow sessions

See [`plan-channel-agnostic-interaction-answering.md`](./plan-channel-agnostic-interaction-answering.md).

### Key Properties

- **Gateway-orchestrated resume**: Use JSON-RPC `interaction.answer` (or shared orchestrator); avoid raw store writes without resume
- **Blocked during orchestration**: Runtime guard prevents `user_ask` when workflow has active children or pending approvals
- **Clear suspension state**: When accepted, `user_ask` suspends as `jsonrpc_spawn_suspended_user_input` (not a normal completion)

### `user_interaction_status` Access Scope

Status reads are scope-checked by the gateway. Access is allowed when either condition is true:

- The caller agent is the interaction owner (`agent_id` matches)
- The caller session belongs to the same `root_session_id` as the interaction

Cross-root, cross-agent status reads return a permission error.

### Example

```json
// user_ask tool response
{
  "ok": true,
  "interaction_id": "ui-4efaa4c6",
  "question": "What format do you want: JSON or CSV?",
  "message": "Waiting for user response..."
}
```

## clarification_needed

### What It Is

A **structured child-to-parent signal** indicating the child agent needs more information to complete its task. Returned as a normal tool result, not a session suspension.

### When To Use

- Child agent lacks critical information to proceed
- Child needs the parent to clarify ambiguous instructions
- Child has encountered an edge case requiring parent decision

### When NOT To Use

- For approval handling (use `workflow_wait`)
- For direct human interaction (use `user_ask` sparingly)
- As a general error signal (use normal error responses)

### How It Works

```
Parent → agent_spawn("coder.default", message="Implement X")

Child → Analyzes task, realizes missing info
     → Returns {"status": "clarification_needed", "question": "What language? Python or JavaScript?"}

Parent → Sees structured result
       → Re-spawns child with clarified instructions
       → agent_spawn("coder.default", message="Implement X in Python")

Child → Proceeds with clarified task
```

### Key Properties

- **Structured**: Machine-readable signal with specific fields
- **No session suspension**: Normal tool result flow
- **Parent-controlled**: Parent decides how to respond
- **Deterministic**: Same input → same clarification request

### Example

```json
// Child agent return value
{
  "status": "clarification_needed",
  "question": "Should I use REST or gRPC for the API?",
  "options": ["REST", "gRPC"],
  "blocking": true
}
```

## Decision Flow

```
┌──────────────────────────────────────────────────────────────┐
│ Does the operation require privileged access?                │
│ (network, install, dangerous commands)                       │
├──────────────────────────────────────────────────────────────┤
│ YES → Use APPROVAL                                           │
│        Tool handles it automatically                         │
│        Gateway suspends and auto-resumes                     │
├──────────────────────────────────────────────────────────────┤
│ NO → Is this a question for a human?                         │
├──────────────────────────────────────────────────────────────┤
│ YES → Is the session orchestrating child tasks?              │
│       ├── YES → DO NOT use user_ask                          │
│       │         Tell user in prose, use workflow_wait        │
│       └── NO → Use user_ask                                  │
│                 Session suspends, explicit resume required   │
├──────────────────────────────────────────────────────────────┤
│ NO → Is this a child agent requesting info from parent?      │
├──────────────────────────────────────────────────────────────┤
│ YES → Return clarification_needed as structured result       │
│       Parent re-spawns with clarified instructions           │
└──────────────────────────────────────────────────────────────┘
```

## Common Mistakes and Fixes

### Mistake 1: Using user_ask for Approval

```
// WRONG: Creates deadlock
user_ask({"question": "Should I approve the network access?"})

// RIGHT: Use workflow_wait
// (Tell user in response text) "Approval apr-xxx pending. Run: gateway approvals approve apr-xxx"
workflow_wait({"task_ids": ["task-xxx"], "timeout_secs": 300})
```

**Why it's wrong**: `user_ask` creates a `UserInputRequired` checkpoint that blocks the session. Workflow join signals can't resume a session blocked on user input. The session is stranded.

### Mistake 2: Changing Payload Between Approval Retries

```
// WRONG: Different payload creates new approval
// Attempt 1: {"capabilities": [{"type": "NetworkAccess", "hosts": ["*"]}]}
// Attempt 2: {"capabilities": [{"type": "NetworkAccess", "hosts": ["*"]}, {"type": "WriteAccess"}]}

// RIGHT: Use stored payload via install_approval_ref
// Attempt 1: Creates approval apr-xxx with stored payload
// Attempt 2: {"capabilities": [...], "promotion_gate": {"install_approval_ref": "apr-xxx"}}
//            Gateway loads stored payload, no drift
```

**Why it's wrong**: Each payload change creates a new approval fingerprint, requiring a new approval. The `agent.install` tool stores the full payload in the approval request for deterministic retry.

### Mistake 3: Using hosts: ["*"] Instead of Detected Domains

```
// WRONG: Overly permissive, inconsistent
{"capabilities": [{"type": "NetworkAccess", "hosts": ["*"]}]}

// RIGHT: Use detected domains from artifact analysis
{"capabilities": [{"type": "NetworkAccess", "hosts": ["api.open-meteo.com", "geocoding-api.open-meteo.com"]}]}
```

**Why it's wrong**: Wildcard hosts violate least-privilege. The `agent.install` tool now auto-detects domains from URL literals in the artifact and shows them in the approval card.

### Mistake 4: Re-spawning Instead of Repairing

```
// WRONG: Infinite retry loop
// Evaluator fails → planner re-spawns evaluator → evaluator fails → ...

// RIGHT: Check error type
// Schema validation error + promotion_record was called → proceed to next step
// Functional failure → iterate with coder
```

**Why it's wrong**: Not all failures are the same. Schema validation errors (LLM response format) are cosmetic if the work was done. Re-spawning wastes tokens and time.

## Runtime Guards

The Gateway enforces several mechanical guards to prevent misuse:

### 1. user_ask Blocked During Orchestration

```rust
// In UserAskTool::execute()
if has_active_children || has_pending_approvals {
    return Ok(json!({
        "ok": false,
        "error": "user_ask is not available while orchestrating workflow tasks. Use workflow_wait."
    }).to_string());
}
```

### 2. Script Entry Validation

```rust
// In AgentInstallTool::execute()
if execution_mode == Script {
    ensure!(script_entry.exists_in_artifact, 
        "script_entry '{}' not found in artifact '{}'", script_entry, artifact_id);
}
```

### 3. Domain-Level Approval Reuse

```rust
// In SandboxExecTool::execute()
let approved = gateway_store.get_approved_approvals_for_root(root_session_id)?;
if approved.iter().any(|r| detected_hosts.iter().any(|h| r.reason.contains(h))) {
    approval_validated_for_command = true; // Skip approval
}
```

## Summary

| Use Case | Mechanism |
|---|---|
| Network access in sandbox_exec | **Approval** (automatic) |
| Agent installation | **Approval** (automatic) |
| Asking user for preferences | **user_ask** (when no active workflow) |
| Child needs more info from parent | **clarification_needed** (structured result) |
| Waiting for approval resolution | **workflow_wait** (NOT user_ask) |
| Telling user about pending approval | **Prose in response text** (NOT user_ask) |

The key principle: **Approval is a Gateway-enforced security mechanism. user_ask is a UI convenience. clarification_needed is a structured agent-to-agent signal.** Never confuse them.
