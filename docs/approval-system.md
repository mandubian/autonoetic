# Approval System Architecture

## Overview

The Autonoetic approval system enforces a **Separation of Powers** between agents (low-privilege reasoners that propose intents) and the Gateway (high-privilege executor that validates and runs them). Before any privileged operation executes, the Gateway requires operator approval.

## Three Distinct Mechanisms

The system has three distinct human-interaction mechanisms that serve different purposes:

| Mechanism | Purpose | Who Resolves | Session Behavior |
|---|---|---|---|
| **Approval** | Gate privileged operations (sandbox exec with network access, agent revision promotion) | Operator via `gateway approvals approve` | Suspends child session; Gateway auto-resumes on approval |
| **user_ask** | Ask human a question (clarification, preferences, choices) | Human via CLI/chat interaction | Suspends session; requires explicit `resume_from_user_interaction` |
| **clarification_needed** | Child agent signals missing info to parent | Parent agent re-spawns child with clarified task | Child returns structured result; parent decides next action |

### Approval

**When it triggers:**
- `sandbox_exec` with remote network access detected (HTTP/HTTPS URLs, socket calls)
- `agent_revision_promote` for high-risk bundles (NetworkAccess, CodeExecution, broad WriteAccess)
- Dangerous operations (sudo, rm -rf, dd, mkfs) — blocked by policy

**When it does NOT trigger (auto-approved):**
- Agents with `NetworkAccess` capability — all remote access patterns auto-approved
- Safe local inspection commands (`pip list`, `pip show`, `npm list`, etc.) — no network needed
- Dependency install redirect — non-NetworkAccess agents get `dependency_layer_required: true` instead of an approval prompt, directing the planner to route through `packager.default`

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
- **Domain-level reuse**: For `sandbox_exec` with artifact-based analysis, once a domain (e.g., `api.open-meteo.com`) is approved at the root workflow level, other sessions under the same root can reuse the approval without re-asking
- **Session approval grants**: Once the operator approves network access to specific hosts for a root session, all subsequent `sandbox_exec` calls within that session targeting the same hosts are auto-approved — no repeated operator prompts
- **Payload preservation**: For `agent_revision_promote`, the full promote args are stored in the approval request; on approval the gateway resumes the suspended turn with the real promotion result, preventing payload drift
- **Deduplication**: Both `sandbox_exec` and `agent_revision_promote` prevent duplicate approval requests — if an approval is already pending (or recently approved) for the same operation, the existing request ID is returned instead of creating a new one

### user_ask

**When it triggers:**
- Agent explicitly calls `user_ask` tool to ask the human a question

**How it works:**
1. Tool creates a `UserInteraction` record
2. Session checkpoints with `YieldReason::UserInputRequired`
3. Returns `TurnOutcome::SuspendedUserInput` (visible as `jsonrpc_spawn_suspended_user_input`)
4. Human answers via CLI/chat
5. Caller must explicitly invoke `resume_from_user_interaction` to resume

**Key properties:**
- **Explicit resume required**: Unlike approval, the session doesn't auto-resume
- **UI primitive, not workflow primitive**: Should NOT be used for approval handling
- **Restricted during orchestration**: `user_ask` is blocked when the session has active workflow children or pending approvals

### clarification_needed

**When it triggers:**
- Child agent (via `agent_spawn`) returns a result indicating it needs more information

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
        detected_hosts: Option<Vec<String>>,
    },
    AgentRevisionPromote {
        agent_id: String,
        revision_id: String,
        summary: String,
        requested_by_agent_id: String,
        promote_fingerprint: String,
        payload: Option<serde_json::Value>,  // Stored for continuation replay
    },
    WriteFile { path: String, requires_approval: bool },
}
```

### Approval Flow for sandbox_exec

```
┌─────────────────────────────────────────────────────────────────┐
│ 1. Agent calls sandbox_exec with artifact containing URLs       │
├─────────────────────────────────────────────────────────────────┤
│ 2. RemoteAccessAnalyzer detects URL literals                    │
│    → Extracts hosts: api.open-meteo.com, api.openweathermap.org │
├─────────────────────────────────────────────────────────────────┤
│ 3. Check for existing approved/pending approvals                │
│    a. Exec cache hit (same fingerprint) → SKIP approval         │
│    b. Session grant covers targets → SKIP approval              │
│    c. Domain already approved at root level → SKIP approval     │
│    d. Pending approval exists → REUSE it                        │
│    e. Otherwise → CREATE new approval request                   │
├─────────────────────────────────────────────────────────────────┤
│ 4. Create ApprovalRequest, persist to SQLite                    │
│    → Store command + detected_hosts in action payload           │
├─────────────────────────────────────────────────────────────────┤
│ 5. Return approval_required response, suspend session           │
├─────────────────────────────────────────────────────────────────┤
│ 6. Operator runs: gateway approvals approve apr-xxx             │
├─────────────────────────────────────────────────────────────────┤
│ 7. Gateway records session approval grants for detected hosts   │
├─────────────────────────────────────────────────────────────────┤
│ 8. Gateway resumes session, executes approved command           │
│    → Injects real sandbox output into conversation              │
└─────────────────────────────────────────────────────────────────┘
```

### Approval Flow for agent_revision_promote

```
┌─────────────────────────────────────────────────────────────────┐
│ 1. specialized_builder calls agent_revision_promote             │
├─────────────────────────────────────────────────────────────────┤
│ 2. Gateway checks AgentRevision capability + eval gating        │
│    → Runs RemoteAccessAnalyzer on bundle artifact files         │
│    → Extracts detected network hosts from URL literals          │
│    → Includes hosts in approval card for operator visibility    │
├─────────────────────────────────────────────────────────────────┤
│ 3. Store full promote args as JSON payload in ApprovalRequest   │
│    → Suspend turn with YieldReason::ApprovalRequired            │
│    → Write continuation file to disk                            │
├─────────────────────────────────────────────────────────────────┤
│ 4. Return approval_required response                            │
│    → "Approval apr-xxx is pending. No retry needed."            │
├─────────────────────────────────────────────────────────────────┤
│ 5. Operator runs: gateway approvals approve apr-xxx             │
├─────────────────────────────────────────────────────────────────┤
│ 6. Gateway resumes suspended session automatically              │
│    → Loads continuation file, executes approved promotion       │
│    → Injects real promote result into conversation              │
└─────────────────────────────────────────────────────────────────┘
```

### Domain-Level Approval Reuse (sandbox_exec)

For `sandbox_exec`, approvals are reusable across sessions under the same root workflow:

1. **First session** runs artifact with `api.open-meteo.com` → approval created → operator approves
2. **Second session** (different agent) runs same artifact → detects same domain → finds existing approved request → **skips approval**, proceeds directly

This works because:
- Artifacts are immutable (same files → same domains)
- The approval reason includes detected domains
- The check queries `get_approved_approvals_for_root()` for matching domains

### Session Approval Grants

When the operator approves a `sandbox_exec` that accesses specific hosts, the gateway creates **session approval grants** — `(root_session_id, host)` pairs stored in SQLite. These grants prevent repeated operator prompts for the same hosts within the same root session.

**How it works:**

1. Operator approves `sandbox_exec` accessing `api.open-meteo.com` and `nominatim.openstreetmap.org`
2. Gateway extracts the detected hosts from the approval action and inserts them as session grants
3. When the same agent (or any other agent in the same root session) calls `sandbox_exec` with code that accesses a **subset** of the already-granted hosts, the gateway auto-approves without operator interaction

**Scope:**
- Grants are scoped to the **root session** — all agents within the root session benefit (e.g., `coder.default` and `evaluator.default` working on the same artifact)
- Grants require **concrete targets** (URL literals or IP addresses) — dynamic URLs that can't be statically resolved don't produce grants
- Grants are cleaned up when the root session ends (completed, failed, or emergency-stopped)
- Grants are **not** cleaned up for suspended sessions (which may resume and still need their grants)

**Why root-session scoping?** Agents within a root session cooperate on the same workflow. If the operator trusts `api.open-meteo.com` for one agent in the session, it should be trusted for all. The `agent_id` is stored per grant row for audit/forensics purposes.

**The approval check order is:**
1. Approved exec cache (fingerprint-level, cross-session)
2. Session approval grants (host-level, within root session)
3. Existing approved/pending approvals (domain-level)
4. New approval request

### Hook-Based Reactive Dispatch

When an approval is resolved (approved, rejected, or cancelled), the gateway's **hook system** can trigger actions:

- **`deliver_signal`** — Dispatches an `ApprovalResolved` signal to the waiting session, resuming the agent's turn. Currently wired through the existing `write_signal` path in `approval.rs`.
- **`publish_report`** — Can be triggered on `approval.resolved` to update observability data.

Hooks are configured in `config.yaml` (see `docs/config-reference.md` → Hooks). The `approval.resolved` hook receives a `HookContext` with `request_id`, `decision`, `session_id`, `agent_id`, and `root_session_id`.

### Promotion Severity Gating

The `promotion_record` tool mechanically enforces that `pass=true` cannot be set when findings indicate the code hasn't been properly validated:

- **Error/Critical findings**: `pass=true` is **rejected** — the gateway refuses to record a passing promotion when any finding has `error` or `critical` severity
- **Warning findings**: `pass=true` is **rejected** unless every warning finding includes a non-empty `evidence` field containing concrete proof (e.g., sandbox output, test results) that the issue was actually investigated — not just an LLM's opinion that the warning is acceptable
- **Info findings**: No restriction — `pass=true` is allowed

This prevents the scenario where an evaluator sets `pass=true` despite code that has never been functionally validated (e.g., all sandbox exec returned 403 errors). The evidence requirement ensures the evaluator must provide factual proof, not just an assertion.

### Continuation-Based Resume (sandbox_exec)

When a `sandbox_exec` approval is resolved:

1. Signal delivered via `event.ingest` with `approval_request_id` in metadata
2. Router extracts `task_id` from the approval request
3. `spawn_agent_once` finds the continuation file on disk
4. `execute_approved_action` runs the approved command with real sandbox isolation
5. Real output (stdout/stderr/exit_code) is injected into the LLM conversation
6. LLM continues with actual results, not re-deriving what to do

### Continuation-Based Resume (agent_revision_promote)

When an `agent_revision_promote` approval is resolved:

1. Signal delivered via `event.ingest` with `approval_request_id` in metadata
2. Router extracts `task_id` from the approval request
3. `spawn_agent_once` finds the continuation file on disk
4. `execute_approved_action` runs the approved promotion
5. Real promotion result is injected into the LLM conversation
6. Agent continues with actual result — no retry or payload re-submission needed

## Approval Deduplication

The approval system prevents **approval flooding** — when an agent retries the same operation, it creates a single `apr-*` request rather than one per retry.

### sandbox_exec Deduplication

Session-Level)

For `sandbox_exec`, deduplication is **session-scoped**:
- Before creating a new approval, the gateway checks if thesession` already has a pending `SandboxExec` approval
- If found, returns the existing `request_id` with `approval_already_pending: true`
- Each child session in a workflow gets its own independent approval (no cross-session dedup)

**Why session-scoped?** Two child sessions under the same root workflow may both need `sandbox_exec` approvals for different commands. If they shared one approval, only one session would get the resume signal.

 Session-scoped dedup ensures each session can be independently approved and resumed.

### agent_revision_promote Deduplication (Root + Session Level)

For `agent_revision_promote`, deduplication mirrors the sandbox pattern:
1. **Pending check** (root + session): Before creating a new approval, checks both the root-level and session-level pending approvals for an `AgentRevisionPromote` with the same `revision_id`. If found, returns the existing pending request.
 Root-level check catches cases where the planner respawns the builder with a new sub-session ID.
 Session-level check catches any redundant promote calls within the same session.

2. **Approved check** (session-level): Since resume is continuation-based (no agent retry), this check exists for edge cases where the builder sends a second promote before the auto-resume arrives. Returns the approved request ID with `already_approved: true` instead of creating a duplicate.

This prevents duplicate approvals when a workflow is restarted or the builder is re-spawned mid-flow.

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
REQUEST ID                            AGENT                KIND              DETAILS
apr-3458926a                          specialized_bui…     revision_promote  promote: weather.default rev-abc123
apr-9e6420c1                          evaluator.defau…     sandbox_exec      exec: cd /tmp && python3 -c "import requests; print(…
```

## Common Pitfalls

### 1. Using user_ask for Approval Handling

**Wrong:**
```
// Planner sees child is awaiting_approval
user_ask({"question": "Should I approve the network access?"})
```

**Right:**
```
// Tell user in prose, then wait
// (In response text): "Approval apr-xxx is pending. Run: gateway approvals approve apr-xxx"
workflow_wait({"task_ids": ["task-xxx"], "timeout_secs": 300})
```

### 2. Submitting a New Promote While One is Pending

**Wrong:**
```
// Turn 1: agent calls agent_revision_promote → receives approval_required: apr-xxx
// Turn 2: agent calls agent_revision_promote AGAIN while apr-xxx is still pending
```

The continuation mechanism handles resume automatically. The agent should wait for the suspended turn to resume — not retry.

**Right:**
```
// Turn 1: agent calls agent_revision_promote → receives approval_required: apr-xxx
// Agent reports to user: "Approval apr-xxx pending. Run: gateway approvals approve apr-xxx"
// After operator approves → gateway auto-resumes the suspended turn with real result
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

The `agent_revision_create` step auto-detects domains from URL literals in the artifact and includes them in the approval card. Use the detected domains for precise, least-privilege capabilities.

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
| `autonoetic-gateway/src/scheduler/approval.rs` | Approval lifecycle, resume logic, signal delivery, pending approval queries, session grant insertion on approval |
| `autonoetic-gateway/src/scheduler/gateway_store/approvals.rs` | Approval persistence (SQLite), session grant CRUD (`insert_session_grant`, `get_session_grants`, `session_grants_cover_targets`, `delete_session_grants`) |
| `autonoetic-gateway/src/runtime/tools/sandbox.rs` | Approval checks in `SandboxExecTool`; session grant check; sandbox deduplication logic |
| `autonoetic-gateway/src/runtime/tools/agent.rs` | Approval checks in `AgentRevisionPromoteTool`; promote deduplication logic |
| `autonoetic-gateway/src/runtime/tools/promotion.rs` | Mechanical severity gating for `promotion_record` — rejects `pass=true` with error/critical findings or warnings without evidence |
| `autonoetic-gateway/src/runtime/remote_access.rs` | URL/domain extraction from code (`RemoteAccessAnalyzer`) |
| `autonoetic-gateway/src/runtime/approved_exec_cache.rs` | Domain normalization (`normalize_targets`), fingerprinting, host normalization (lowercase + trailing dot stripping) |
| `autonoetic-gateway/src/runtime/lifecycle.rs` | Session close — grant cleanup for non-suspended sessions |
| `autonoetic-gateway/src/execution.rs` | Emergency stop — grant cleanup during circuit breaker |
| `autonoetic-gateway/src/scheduler/gateway_store/migrate.rs` | Database migration v4: `session_approval_grants` table |
| `autonoetic-gateway/src/scheduler/hooks.rs` | Hook system — configurable reactive dispatch (`publish_report`, `deliver_signal`). Future: hook-based approval auto-resolution |
| `autonoetic-gateway/src/scheduler/gateway_store/migrate.rs` | Database migration v7: `published_session_reports`, `published_session_reports_fts`, `hook_deliveries` tables |
| `autonoetic-types/src/background.rs` | `ApprovalRequest`, `ScheduledAction` (with `detected_hosts`), `ApprovalStatus` types |
| `autonoetic-types/src/hooks.rs` | `HookEvent`, `HookAction`, `HookConfig`, `HookContext` types |
| `autonoetic-types/src/promotion.rs` | `PromotionRecordArgs` type |
