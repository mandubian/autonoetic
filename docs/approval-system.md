# Approval System Architecture

## Overview

The Autonoetic approval system enforces a **Separation of Powers** between agents (low-privilege reasoners that propose intents) and the Gateway (high-privilege executor that validates and runs them). Before any privileged operation executes, the Gateway requires operator approval.

## Three Distinct Mechanisms

The system has three distinct human-interaction mechanisms that serve different purposes:

| Mechanism | Purpose | Who Resolves | Session Behavior |
|---|---|---|---|
| **Approval** | Gate privileged operations (sandbox exec with network access, agent install) | Operator via `gateway approvals approve` | Suspends child session; Gateway auto-resumes on approval |
| **user.ask** | Ask human a question (clarification, preferences, choices) | Human via CLI/chat interaction | Suspends session; requires explicit `resume_from_user_interaction` |
| **clarification_needed** | Child agent signals missing info to parent | Parent agent re-spawns child with clarified task | Child returns structured result; parent decides next action |

### Approval

**When it triggers:**
- `sandbox.exec` with remote network access detected (HTTP/HTTPS URLs, socket calls)
- `agent.install` for evolution roles (specialized_builder)
- Dangerous operations (sudo, rm -rf, dd, mkfs) — blocked by policy

**How it works:**
1. Tool detects the operation requires approval
2. Creates an `ApprovalRequest` persisted to SQLite
3. Returns `{"ok": false, "approval_required": true, "request_id": "apr-xxx", ...}`
4. Session checkpoints with `YieldReason::ApprovalRequired`
5. Operator runs `gateway approvals approve apr-xxx`
6. Gateway resumes the suspended session automatically
7. The approved action executes with real results injected into the conversation

**Key properties:**
- **Durable**: Approval requests persist to SQLite
- **Auto-resume**: Gateway automatically resumes the session when approval is resolved
- **Domain-level reuse**: For `sandbox.exec` with artifact-based analysis, once a domain (e.g., `api.open-meteo.com`) is approved at the root workflow level, other sessions under the same root can reuse the approval without re-asking
- **Payload preservation**: For `agent.install`, the full install args are stored in the approval request and replayed on retry, preventing payload drift
- **Deduplication**: Both `sandbox.exec` and `agent.install` prevent duplicate approval requests — if an approval is already pending (or recently approved) for the same operation, the existing request ID is returned instead of creating a new one

### user.ask

**When it triggers:**
- Agent explicitly calls `user.ask` tool to ask the human a question

**How it works:**
1. Tool creates a `UserInteraction` record
2. Session checkpoints with `YieldReason::UserInputRequired`
3. Returns `TurnOutcome::SuspendedUserInput` (visible as `jsonrpc_spawn_suspended_user_input`)
4. Human answers via CLI/chat
5. Caller must explicitly invoke `resume_from_user_interaction` to resume

**Key properties:**
- **Explicit resume required**: Unlike approval, the session doesn't auto-resume
- **UI primitive, not workflow primitive**: Should NOT be used for approval handling
- **Restricted during orchestration**: `user.ask` is blocked when the session has active workflow children or pending approvals

### clarification_needed

**When it triggers:**
- Child agent (via `agent.spawn`) returns a result indicating it needs more information

**How it works:**
1. Child agent returns `{"status": "clarification_needed", "question": "..."}`
2. Parent agent sees the structured result
3. Parent re-spawns the child with clarified instructions

**Key properties:**
- **Structured**: Machine-readable signal from child to parent
- **Parent-controlled**: Parent decides how to respond
- **No session suspension**: Normal tool result flow

## Approval Deep Dive

### Approval Request Structure

```rust
pub struct ApprovalRequest {
    pub request_id: String,              // "apr-xxx"
    pub agent_id: String,                // Agent that requested approval
    pub session_id: String,              // Session that triggered approval
    pub root_session_id: Option<String>, // Root workflow session
    pub action: ScheduledAction,         // What operation needs approval
    pub created_at: String,              // RFC3339 timestamp
    pub status: Option<ApprovalStatus>,  // None = pending, Some(Approved/Rejected)
    pub decided_at: Option<String>,
    pub decided_by: Option<String>,
    pub reason: Option<String>,          // Why approval is needed
    pub evidence_ref: Option<String>,
    pub workflow_id: Option<String>,     // Workflow this belongs to
    pub task_id: Option<String>,         // Task this belongs to
}
```

### ScheduledAction Types

```rust
pub enum ScheduledAction {
    SandboxExec {
        command: String,
        dependencies: Option<ScheduledActionDependencies>,
        requires_approval: bool,
    },
    AgentInstall {
        agent_id: String,
        summary: String,
        requested_by_agent_id: String,
        install_fingerprint: String,
        payload: Option<serde_json::Value>,  // Stored for deterministic retry
    },
    WriteFile { path: String, requires_approval: bool },
}
```

### Approval Flow for sandbox.exec

```
┌─────────────────────────────────────────────────────────────────┐
│ 1. Agent calls sandbox.exec with artifact containing URLs       │
├─────────────────────────────────────────────────────────────────┤
│ 2. RemoteAccessAnalyzer detects URL literals                    │
│    → Extracts hosts: api.open-meteo.com, api.openweathermap.org │
├─────────────────────────────────────────────────────────────────┤
│ 3. Check for existing approved/pending approvals                │
│    → If domain already approved at root level → SKIP approval   │
│    → If pending approval exists → REUSE it                      │
│    → Otherwise → CREATE new approval request                    │
├─────────────────────────────────────────────────────────────────┤
│ 4. Create ApprovalRequest, persist to SQLite                    │
│    → Store command, detected hosts in reason field              │
├─────────────────────────────────────────────────────────────────┤
│ 5. Return approval_required response, suspend session           │
├─────────────────────────────────────────────────────────────────┤
│ 6. Operator runs: gateway approvals approve apr-xxx             │
├─────────────────────────────────────────────────────────────────┤
│ 7. Gateway resumes session, executes approved command           │
│    → Injects real sandbox output into conversation              │
└─────────────────────────────────────────────────────────────────┘
```

### Approval Flow for agent.install

```
┌─────────────────────────────────────────────────────────────────┐
│ 1. Specialized builder calls agent.install                      │
├─────────────────────────────────────────────────────────────────┤
│ 2. Resolve artifact files, run RemoteAccessAnalyzer             │
│    → Extract detected network hosts from URL literals           │
│    → Include hosts in approval card for operator visibility     │
├─────────────────────────────────────────────────────────────────┤
│ 3. Store full install args as JSON payload in ApprovalRequest   │
│    → Ensures deterministic retry (no payload drift)             │
├─────────────────────────────────────────────────────────────────┤
│ 4. Return approval_required with retry instructions             │
│    → "Retry with promotion_gate.install_approval_ref = 'apr-xxx'"│
├─────────────────────────────────────────────────────────────────┤
│ 5. Operator approves                                            │
├─────────────────────────────────────────────────────────────────┤
│ 6. Builder retries with install_approval_ref                    │
│    → Gateway loads stored payload from approval request         │
│    → Replaces args with stored payload (deterministic)          │
│    → Proceeds with install                                      │
└─────────────────────────────────────────────────────────────────┘
```

### Domain-Level Approval Reuse (sandbox.exec)

For `sandbox.exec`, approvals are reusable across sessions under the same root workflow:

1. **First session** runs artifact with `api.open-meteo.com` → approval created → operator approves
2. **Second session** (different agent) runs same artifact → detects same domain → finds existing approved request → **skips approval**, proceeds directly

This works because:
- Artifacts are immutable (same files → same domains)
- The approval reason includes detected domains
- The check queries `get_approved_approvals_for_root()` for matching domains

### Continuation-Based Resume (sandbox.exec)

When a `sandbox.exec` approval is resolved:

1. Signal delivered via `event.ingest` with `approval_request_id` in metadata
2. Router extracts `task_id` from the approval request
3. `spawn_agent_once` finds the continuation file on disk
4. `execute_approved_action` runs the approved command with real sandbox isolation
5. Real output (stdout/stderr/exit_code) is injected into the LLM conversation
6. LLM continues with actual results, not re-deriving what to do

### Deterministic Retry (agent.install)

When an `agent.install` approval is resolved:

1. Builder retries with `promotion_gate.install_approval_ref = "apr-xxx"`
2. Gateway looks up the approval request by ID
3. Loads the stored JSON payload from the approval request
4. Replaces the current args with the stored payload
5. This ensures the install uses the EXACT same capabilities, instructions, etc.
6. No payload drift between attempts

## Approval Deduplication

The approval system prevents **approval flooding** — when an agent retries the same operation, it creates a single `apr-*` request rather than one per retry.

### sandbox.exec Deduplication

Session-Level)

For `sandbox.exec`, deduplication is **session-scoped**:
- Before creating a new approval, the gateway checks if thesession` already has a pending `SandboxExec` approval
- If found, returns the existing `request_id` with `approval_already_pending: true`
- Each child session in a workflow gets its own independent approval (no cross-session dedup)

**Why session-scoped?** Two child sessions under the same root workflow may both need `sandbox.exec` approvals for different commands. If they shared one approval, only one session would get the resume signal.

 Session-scoped dedup ensures each session can be independently approved and resumed.

### agent.install Deduplication (Root + Session Level)

For `agent.install`, deduplication is more aggressive:
1. **Pending check** (root + session): Before creating a new approval, checks both the root-level and session-level pending approvals for an `AgentInstall` with the same `agent_id`. If found, returns the existing pending request.
 Root-level check catches cases where the planner respawns the builder with a new sub-session ID.
 Session-level check catches retries within the same session.

2. **Approved check** (session-level): Even after approval is granted, if the builder retries without `install_approval_ref`, the gateway checks session-level for recently-approved `AgentInstall` approvals for the same `agent_id`. If found, returns the approved request ID with `already_approved: true` instead of creating a duplicate.

This two-layer approach prevents the bug where the builder's retry loop creates 4+ duplicate approvals (one per turn) after the operator already approved the first one.

### Approval Reason Field (Enhanced)

The `reason` field on approval requests now includes extracted host information when available:

```
# Before
"reason": "Remote access detected: Detected 1 remote access pattern(s) in categories: import"

# After (when URL literals are found)
"reason": "Remote access detected: Detected 3 patterns in categories: import, url_literal → hosts: api.open-meteo.com, geocoding-api.open-meteo.com"
```

This gives operators immediate visibility into **what domains** the code will access, without needing to inspect the full command.

### CLI: Enhanced Approval List

The `gateway approvals list` command now shows actionable details:

```
REQUEST ID                            AGENT                KIND           DETAILS
apr-3458926a                          specialized_bui…     agent_install  install: weather.default (weather.default with NetworkAccess)
apr-9e6420c1                          evaluator.defau…     sandbox_exec  exec: cd /tmp && python3 -c "import requests; print(…
```

## Common Pitfalls

### 1. Using user.ask for Approval Handling

**Wrong:**
```
// Planner sees child is awaiting_approval
user.ask({"question": "Should I approve the network access?"})
```

**Right:**
```
// Tell user in prose, then wait
// (In response text): "Approval apr-xxx is pending. Run: gateway approvals approve apr-xxx"
workflow.wait({"task_ids": ["task-xxx"], "timeout_secs": 300})
```

### 2. Changing Payload Between Approval Retries

**Wrong:**
```json
// First attempt
{"agent_id": "weather", "capabilities": [{"type": "NetworkAccess", "hosts": ["*"]}]}
// Second attempt (CHANGED)
{"agent_id": "weather", "capabilities": [{"type": "NetworkAccess", "hosts": ["*"]}, {"type": "WriteAccess", "scopes": ["self.*"]}]}
```

**Right:**
```json
// First attempt
{"agent_id": "weather", "capabilities": [{"type": "NetworkAccess", "hosts": ["api.open-meteo.com"]}]}
// Second attempt (EXACT same + approval_ref)
{"agent_id": "weather", "capabilities": [{"type": "NetworkAccess", "hosts": ["api.open-meteo.com"]}], "promotion_gate": {"install_approval_ref": "apr-xxx"}}
```

### 3. Using hosts: ["*"] Instead of Detected Domains

**Wrong:**
```json
{"capabilities": [{"type": "NetworkAccess", "hosts": ["*"]}]}
```

**Right:**
```json
{"capabilities": [{"type": "NetworkAccess", "hosts": ["api.open-meteo.com", "geocoding-api.open-meteo.com"]}]}
```

The `agent.install` tool now auto-detects domains from URL literals in the artifact and includes them in the approval card. Use the detected domains for precise, least-privilege capabilities.

## CLI Commands

```bash
# List pending approvals
autonoetic gateway approvals list --config /path/to/config.yaml

# Approve a request
autonoetic gateway approvals approve apr-xxx --config /path/to/config.yaml

# Reject a request
autonoetic gateway approvals reject apr-xxx --config /path/to/config.yaml

# Show approval details
autonoetic gateway approvals show apr-xxx --config /path/to/config.yaml
```

## Implementation Files

| File | Role |
|---|---|
| `autonoetic-gateway/src/scheduler/approval.rs` | Approval lifecycle, resume logic, signal delivery, pending approval queries (`pending_approval_requests_for_root`, `pending_approval_requests_for_session`, `pending_sandbox_exec_requests_for_session`) |
| `autonoetic-gateway/src/scheduler/gateway_store.rs` | Approval persistence (SQLite), queries (`get_approved_approvals_for_root`, `get_approved_approvals_for_session`) |
| `autonoetic-gateway/src/runtime/tools.rs` | Approval checks in `SandboxExecTool` and `AgentInstallTool`; deduplication logic for both tools |
| `autonoetic-gateway/src/runtime/remote_access.rs` | URL/domain extraction from code (`RemoteAccessAnalyzer`) |
| `autonoetic-gateway/src/runtime/approved_exec_cache.rs` | Domain normalization (`normalize_targets`), fingerprinting |
| `autonoetic-types/src/background.rs` | `ApprovalRequest`, `ScheduledAction`, `ApprovalStatus` types |
