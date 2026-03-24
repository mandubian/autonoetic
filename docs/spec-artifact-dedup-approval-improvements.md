# Spec: Artifact Deduplication and Approval Process Improvements

**Status:** Draft
**Author:** Architecture Review
**Date:** 2026-03-23

---

## 1. Problem Statement

### 1.1 Artifact Pollution

**Current State:**
- Content is deduplicated at the content store level (SHA-256 handles)
- Artifacts are NOT deduplicated - same inputs create different artifact IDs
- Multiple agents may create duplicate artifacts for the same logical content
- No reuse mechanism when an artifact already exists for the same purpose

**Observed Issues:**
```
Agent A calls artifact.build(["design.md", "code.py"]) → art_aaa111
Agent B calls artifact.build(["design.md", "code.py"]) → art_bbb222  (duplicate!)
```

**Consequences:**
- Storage bloat
- Artifact ID proliferation
- Confusion about which artifact to use
- Evaluator/auditor may review different artifacts than the one being installed

### 1.2 Approval Process Brittleness

**Current Model: "Dumb Gate / Agent Retry"**

```
Tool needs approval → Returns approval_required: true → Agent stops
User approves → Gateway notifies → Agent retries with approval_ref
```

**Observed Issues:**

| Issue | Description | Impact |
|-------|-------------|--------|
| **Manual retry** | Agent must re-run tool with approval_ref | Friction, user error potential |
| **State fragmentation** | SQLite + filesystem + in-memory stores | Inconsistency risk |
| **Race conditions** | Multiple approvals in same session | Competing requests |
| **Agent lifecycle** | Restart loses approval signal | Orphaned approvals |
| **Limited visibility** | No query tool for approval status | Poor UX |
| **Blocking behavior** | agent.spawn blocked during pending approval | Workflow stalls |
| **No cleanup** | Old approvals never removed | Storage growth |
| **String matching** | Session ID parsing is fragile | Edge case failures |

---

## 2. Artifact Deduplication Solution

### 2.1 Design Principle

**Artifact identity = deterministic hash of inputs**

Same inputs → same artifact ID → reuse existing artifact

### 2.2 Implementation

#### 2.2.1 Deterministic Artifact ID

Replace UUID-based artifact IDs with content-addressed IDs:

```rust
// Current (non-deterministic):
let artifact_id = format!("art_{}", uuid::Uuid::new_v4().simple());

// Proposed (deterministic):
fn compute_artifact_id(inputs: &[ContentHandle], entrypoints: &[String]) -> String {
    let mut hasher = Sha256::new();
    // Sort inputs for determinism
    let mut sorted_inputs = inputs.to_vec();
    sorted_inputs.sort();
    for handle in sorted_inputs {
        hasher.update(handle.as_bytes());
    }
    // Sort entrypoints for determinism
    let mut sorted_entrypoints = entrypoints.to_vec();
    sorted_entrypoints.sort();
    for ep in sorted_entrypoints {
        hasher.update(ep.as_bytes());
    }
    let hash = hasher.finalize();
    format!("art_{:x}", hash)  // e.g., "art_a1b2c3d4e5f6..."
}
```

#### 2.2.2 Idempotent artifact.build

```rust
pub fn execute_artifact_build(&self, args: &ArtifactBuildArgs) -> Result<ArtifactBuildResponse> {
    // Resolve inputs to handles
    let handles = self.resolve_inputs(&args.inputs)?;

    // Compute deterministic artifact ID
    let artifact_id = compute_artifact_id(&handles, &args.entrypoints);

    // Check if artifact already exists
    if let Some(existing) = self.artifact_store.get(&artifact_id)? {
        return Ok(ArtifactBuildResponse {
            artifact_id: existing.artifact_id,
            created_at: existing.created_at,
            digest: existing.digest,
            reused: true,  // NEW: indicates reuse
        });
    }

    // Create new artifact only if not exists
    let artifact = self.create_artifact(artifact_id, handles, args)?;
    Ok(ArtifactBuildResponse {
        artifact_id: artifact.artifact_id,
        created_at: artifact.created_at,
        digest: artifact.digest,
        reused: false,
    })
}
```

#### 2.2.3 Response Enhancement

```json
{
  "artifact_id": "art_a1b2c3d4",
  "created_at": "2026-03-23T14:00:00Z",
  "digest": "sha256:abc123...",
  "reused": false,  // NEW
  "files": ["main.py", "SKILL.md"],
  "entrypoints": ["main.py"]
}
```

When reused:
```json
{
  "artifact_id": "art_a1b2c3d4",
  "created_at": "2026-03-23T10:00:00Z",  // Original creation time
  "reused": true,  // Indicates this was an existing artifact
  "message": "Reused existing artifact with same inputs"
}
```

### 2.3 Benefits

| Benefit | Description |
|---------|-------------|
| **No duplicate artifacts** | Same inputs always produce same ID |
| **Automatic reuse** | No agent logic changes needed |
| **Consistent references** | All agents use same artifact ID |
| **Reduced storage** | No redundant artifact manifests |
| **Clear lineage** | Artifact ID is meaningful (content hash) |

### 2.4 Edge Cases

| Case | Behavior |
|------|----------|
| Same content, different order | Same ID (sorted before hashing) |
| Same content, different entrypoints | Different ID (entrypoints in hash) |
| Content modified after artifact creation | New artifact (content handles changed) |
| Concurrent creation of same artifact | First wins, others get existing |

---

## 3. Approval Process Improvements

### 3.1 Design Principle

**"Smart Gate / Auto-Resume" model**

```
Tool needs approval → Returns pending state → Agent hibernates
User approves → Gateway auto-executes → Agent continues with result
```

Key changes:
1. Gateway stores the pending tool call (not just notification)
2. After approval, gateway executes the tool automatically
3. Agent receives the tool result (not just "approved" signal)
4. Agent continues from where it left off

### 3.2 Pending Tool Call Storage

#### 3.2.1 New Data Structure

```rust
/// Represents a tool call waiting for approval
#[derive(Debug, Serialize, Deserialize)]
pub struct PendingToolCall {
    pub pending_id: String,           // Unique ID for this pending call
    pub approval_request_id: String,  // Linked approval request
    pub tool_name: String,
    pub tool_args: serde_json::Value,
    pub session_id: String,
    pub agent_id: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,    // Auto-cleanup
}

/// Result of approval resolution
#[derive(Debug, Serialize, Deserialize)]
pub struct ApprovalResolution {
    pub approval_request_id: String,
    pub status: ApprovalStatus,       // Approved, Rejected, Expired
    pub tool_result: Option<serde_json::Value>,  // Executed result if approved
    pub error: Option<String>,        // Error if rejected/failed
}
```

#### 3.2.2 Storage Location

```sql
-- New table in gateway.db
CREATE TABLE pending_tool_calls (
    pending_id TEXT PRIMARY KEY,
    approval_request_id TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    tool_args TEXT NOT NULL,  -- JSON
    session_id TEXT NOT NULL,
    agent_id TEXT NOT NOT,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    FOREIGN KEY (approval_request_id) REFERENCES approvals(request_id)
);

CREATE INDEX idx_pending_session ON pending_tool_calls(session_id);
CREATE INDEX idx_pending_approval ON pending_tool_calls(approval_request_id);
```

### 3.3 Auto-Execution After Approval

#### 3.3.1 Approval Resolution Flow

```
┌─────────────────────────────────────────────────────────────┐
│  User calls: autonoetic gateway approvals approve <id>      │
│       ↓                                                      │
│  Gateway loads approval request                              │
│       ↓                                                      │
│  Gateway loads pending_tool_call linked to approval         │
│       ↓                                                      │
│  Gateway EXECUTES the tool call automatically               │
│       ↓                                                      │
│  Gateway stores result in approval_resolution               │
│       ↓                                                      │
│  Gateway notifies agent: "Your tool call completed"         │
│       ↓                                                      │
│  Agent wakes, receives tool result, continues               │
└─────────────────────────────────────────────────────────────┘
```

#### 3.3.2 Implementation

```rust
pub fn resolve_approval(
    &self,
    request_id: &str,
    decision: ApprovalDecision,  // Approve or Reject
) -> Result<ApprovalResolution> {
    // Load approval request
    let approval = self.approval_store.get(request_id)?;

    // Load pending tool call
    let pending = self.pending_store.get_by_approval(request_id)?;

    // Execute tool if approved
    let tool_result = if decision == ApprovalDecision::Approve {
        Some(self.execute_tool(
            &pending.tool_name,
            &pending.tool_args,
            &pending.session_id,
        )?)
    } else {
        None
    };

    // Create resolution
    let resolution = ApprovalResolution {
        approval_request_id: request_id.to_string(),
        status: if decision == ApprovalDecision::Approve {
            ApprovalStatus::Approved
        } else {
            ApprovalStatus::Rejected
        },
        tool_result,
        error: if decision == ApprovalDecision::Reject {
            Some("Approval rejected by user".to_string())
        } else {
            None
        },
    };

    // Store resolution for agent to retrieve
    self.resolution_store.put(&resolution)?;

    // Notify agent
    self.notify_agent(&pending.session_id, &resolution)?;

    // Cleanup pending
    self.pending_store.delete(&pending.pending_id)?;

    Ok(resolution)
}
```

### 3.4 Agent Wake with Tool Result

#### 3.4.1 Enhanced Session Resume

When agent wakes after approval:

```json
// Current (brittle):
{
  "type": "approval_resolved",
  "approval_id": "apr-abc123",
  "status": "approved"
}
// Agent must retry tool call

// Proposed (seamless):
{
  "type": "tool_completed",
  "pending_id": "pnd-xyz789",
  "tool_name": "agent.install",
  "result": {
    "artifact_id": "art_f6316ff7",
    "agent_id": "weather-fetcher",
    "installed": true
  }
}
// Agent has the result, continues immediately
```

### 3.5 Approval Status Query Tool

New tool for agents to check approval status:

```json
{
  "name": "approval.status",
  "description": "Query the status of a pending approval request",
  "parameters": {
    "approval_id": {
      "type": "string",
      "description": "The approval request ID to check"
    }
  }
}
```

Response:
```json
{
  "approval_id": "apr-abc123",
  "status": "pending",  // pending, approved, rejected, expired
  "created_at": "2026-03-23T14:00:00Z",
  "expires_at": "2026-03-23T18:00:00Z",
  "resolution": null  // Populated if resolved
}
```

### 3.6 Automatic Cleanup

```rust
/// Run periodically (e.g., every hour)
pub fn cleanup_expired_approvals(&self) -> Result<CleanupStats> {
    let now = Utc::now();

    // Find expired pending tool calls
    let expired = self.pending_store.list_expired(now)?;

    let mut stats = CleanupStats::default();
    for pending in expired {
        // Delete pending
        self.pending_store.delete(&pending.pending_id)?;
        stats.pending_deleted += 1;

        // Mark approval as expired
        if let Some(approval) = self.approval_store.get(&pending.approval_request_id)? {
            if approval.status == ApprovalStatus::Pending {
                self.approval_store.update_status(
                    &approval.request_id,
                    ApprovalStatus::Expired,
                )?;
                stats.approvals_expired += 1;
            }
        }
    }

    // Cleanup old resolved approvals (older than 7 days)
    let old_threshold = now - chrono::Duration::days(7);
    let old_resolutions = self.resolution_store.list_before(old_threshold)?;
    for resolution in old_resolutions {
        self.resolution_store.delete(&resolution.approval_request_id)?;
        stats.resolutions_deleted += 1;
    }

    Ok(stats)
}
```

### 3.7 Reduced Blocking

#### 3.7.1 Allow Async Operations During Pending Approval

Current behavior: `agent.spawn` is completely blocked while any approval is pending.

Proposed: Allow async spawns (already queued) to continue, only block synchronous spawns:

```rust
// Current (too restrictive):
if !args.r#async {
    let pending = pending_approval_requests_for_root(...)?;
    if !pending.is_empty() {
        return Err("Cannot delegate while approval(s) are pending");
    }
}

// Proposed (more permissive):
// Async spawns are NEVER blocked (they queue independently)
// Sync spawns check if THIS SPECIFIC approval blocks the operation
if !args.r#async {
    let blocking = approvals_blocking_session(&session_id)?;
    if !blocking.is_empty() {
        return Err(format!(
            "Cannot delegate: {} approval(s) must be resolved first",
            blocking.len()
        ));
    }
}
```

#### 3.7.2 Approval Categories

Some approvals are more blocking than others:

| Category | Blocks | Example |
|----------|--------|---------|
| `session` | Only same session | sandbox.exec for this agent |
| `workflow` | All sessions in workflow | agent.install affects workflow |
| `global` | Everything | Dangerous system operation |

```yaml
# In tool configuration
tools:
  sandbox.exec:
    approval_category: session
  agent.install:
    approval_category: workflow
  system.shutdown:
    approval_category: global
```

### 3.8 Agent Resumption Protocol (CRITICAL)

#### 3.8.1 The Resumption Problem

**Observed Failure Mode:**

```
1. Coder calls sandbox.exec → FAILS (security)
2. Approval triggered → Agent hibernates
3. User approves → Gateway auto-executes sandbox.exec
4. Agent wakes with result
5. Agent doesn't know what to do → Returns "done" without building artifact
6. Planner sees no artifact → Restarts from scratch
7. New coder → same loop → infinite approval loop
```

**Root Cause:** Agents don't have a mental model for "resumption after interruption". They receive a tool result without context of what they were doing.

#### 3.8.2 Resumption Context Format

When an agent wakes after hibernation, the tool result includes resumption metadata:

```json
{
  "resumed": true,
  "hibernation_reason": "approval_required",
  "original_tool": "sandbox.exec",
  "original_args": {"command": "python3 test.py"},
  "approval_id": "apr-abc123",
  "result": {
    "exit_code": 0,
    "stdout": "All tests passed",
    "stderr": ""
  },
  "message": "Your tool call was approved and executed. Continue from where you left off."
}
```

#### 3.8.3 SKILL.md Guidance for Resumption

All agents should include this guidance in their SKILL.md:

```markdown
## ⚠️ RESUMPTION CHECKLIST (After Hibernation/Approval)

When you wake up after hibernation (approval, timeout, etc.), run this checklist BEFORE taking any action:

### Step 1: Identify Why You Woke Up

Check the tool result for `resumed: true` or an approval resolution message.

### Step 2: Check Your Original Goal

Look at your **first message** - what were you asked to do?

### Step 3: Check Your Progress

Look at your **conversation history** - what steps did you complete?

### Step 4: Determine Next Step

Continue from where you left off - DO NOT restart your task.

### ⚠️ NEVER EndTurn immediately after resumption

Always verify your original goal is complete before ending.
```

#### 3.8.4 Implemented SKILL.md Updates (2026-03-23)

The following agents have been updated with resumption checklists:

| Agent | Changes |
|-------|---------|
| `coder.default` | Added prominent "⚠️ RESUMPTION CHECKLIST" section with table of scenarios |
| `planner.default` | Added resumption checklist with workflow.wait guidance |
| `specialized_builder.default` | Added resumption checklist + clarified `remote_access_detected` |

Key improvements made:
1. **Prominent placement** - Checklist at top of SKILL.md, not buried
2. **Actionable table** - "If X, then Y" format for common scenarios
3. **Explicit warning** - "NEVER EndTurn immediately after resumption"

#### 3.8.4 Conversation History as Checkpoint

**Key Insight:** The agent's conversation history IS its checkpoint. No separate checkpoint mechanism needed.

When agent hibernates:
```
Agent: "I need to build an artifact for weather-fetcher"
Agent: "First, I'll test the code with sandbox.exec"
Agent calls sandbox.exec → HIBERNATE
```

When agent wakes:
```
[System: resumed: true, tool: sandbox.exec, result: tests passed]
Agent looks at history → "I was building an artifact"
Agent continues → "Tests passed, now I'll call artifact.build"
```

#### 3.8.5 Gateway Responsibility (Dumb)

The gateway provides minimal context:
- `resumed: true` - indicates this is a continuation
- `original_tool` - which tool was called
- `result` - the tool's output

The gateway does NOT:
- Track agent intent
- Decide what agent should do next
- Modify agent behavior

#### 3.8.6 Agent Responsibility (Smart)

The agent must:
- Understand `resumed: true` means continuation
- Look at its history to understand context
- Continue its plan without restarting

This is a **training issue**, not a gateway issue. Agents need explicit SKILL.md guidance.

### 3.9 Summary of Approval Improvements

| Issue | Current | Proposed |
|-------|---------|----------|
| Manual retry | Agent must retry | Auto-execute, deliver result |
| State fragmentation | Multiple stores | Single pending_tool_call table |
| Race conditions | Possible | Linked pending→approval, atomic |
| Agent lifecycle | Signal lost | Result stored, retrieved on wake |
| Limited visibility | CLI only | approval.status tool |
| Blocking behavior | All spawns blocked | Only sync spawns, by category |
| No cleanup | Never | Periodic expiration |
| String matching | Fragile | Proper ID references |
| **Resumption context** | **None** | **`resumed: true` with history-based continuation** |

---

## 4. Implementation Plan

### Phase 1: Agent SKILL.md Updates (Week 1) - CRITICAL FIRST

**Do this first to fix the resumption problem**

1. Add resumption handling guidance to all agent SKILL.mds:
   - `agents/specialists/coder.default/SKILL.md`
   - `agents/lead/planner.default/SKILL.md`
   - `agents/research/researcher.default/SKILL.md`
   - `agents/evolution/specialized_builder.default/SKILL.md`
   - All other agents that may hibernate

2. Test with existing approval flow:
   - Trigger approval
   - Approve
   - Verify agent continues correctly (doesn't restart)

### Phase 2: Artifact Deduplication (Week 1-2)

1. Implement `compute_artifact_id()` with deterministic hashing
2. Modify `artifact.build` to check for existing artifacts
3. Add `reused` field to response
4. Add tests for deduplication
5. Update documentation

### Phase 3: Resumption Protocol in Gateway (Week 2-3)

1. Modify tool result format to include `resumed: true` when applicable
2. Include `original_tool`, `original_args`, `hibernation_reason`
3. Add `message` field with continuation hint
4. Add tests for resumption format

### Phase 4: Approval Auto-Execution (Week 3-4)

1. Create `pending_tool_calls` table and store
2. Modify tools to store pending calls when approval needed
3. Implement auto-execution on approval resolution
4. Modify agent wake to deliver tool result with resumption context
5. Add tests for auto-execution flow

### Phase 5: Approval Cleanup & Visibility (Week 4)

1. Implement `approval.status` tool
2. Implement periodic cleanup job
3. Add approval categories for blocking
4. Add tests for cleanup and visibility

---

## 5. Lessons Learned from Previous Auto-Execution Attempt

### 5.1 What Was Tried

Auto-execution was previously implemented in the gateway:
- When tool needed approval, gateway stored the pending call
- After approval, gateway executed the tool automatically
- Result was delivered to the agent on wake

### 5.2 Why It Failed

**Agent Behavior Issue, Not Gateway Issue:**

```
Coder workflow:
1. "I need to build an artifact"
2. Write code files
3. Call sandbox.exec to test → APPROVAL NEEDED
4. [HIBERNATE]
5. [APPROVED, AUTO-EXECUTED]
6. Wake with sandbox.exec result
7. ??? (Agent doesn't know what to do)
8. Return "done" without building artifact
9. Planner confused, restarts from scratch
```

**Root Cause:** Agents treated the resumed result as a new request, not a continuation. They didn't:
- Look at their conversation history
- Understand they were in the middle of a task
- Continue to the next step (artifact.build)

### 5.3 Key Insight

> **Auto-execution is a gateway feature, but resumption is an agent behavior.**

The gateway can execute and deliver results, but agents must know how to continue.

### 5.4 The Fix

Two-part solution:

1. **Gateway:** Provide resumption context (`resumed: true`, `original_tool`, etc.)
2. **Agent:** SKILL.md guidance to look at history and continue

The SKILL.md guidance is MORE IMPORTANT than the gateway changes. Without it, agents will always restart on resumption.

### 5.5 Implementation Order

```
WRONG ORDER (what we tried):
1. Implement auto-execution in gateway
2. Hope agents figure it out
3. Agents fail, infinite loops

RIGHT ORDER:
1. Add resumption guidance to agent SKILL.mds
2. Test with existing flow (manual retry)
3. Verify agents continue correctly
4. Then add auto-execution (optional enhancement)
```

---

## 6. Open Questions

1. **Artifact ID format**: Should we keep `art_` prefix or use pure hash?
   - Option A: `art_a1b2c3d4` (current style, recognizable)
   - Option B: `sha256:a1b2c3d4...` (pure content-addressed)

2. **Approval expiration**: What's the right TTL?
   - Option A: 4 hours (short, forces quick resolution)
   - Option B: 24 hours (allows human review cycles)
   - Option C: Configurable per gateway

3. **Failed auto-execution**: What if tool fails after approval?
   - Option A: Return error to agent, agent handles
   - Option B: Retry once, then return error
   - Option C: Create new approval for retry

4. **Backward compatibility**: Existing pending approvals during migration?
   - Option A: Force resolution before upgrade
   - Option B: Migrate with "manual retry" flag

---

## 6. Success Metrics

| Metric | Current | Target |
|--------|---------|--------|
| Duplicate artifacts per session | 2-5 | 0 |
| Approval resolution time (user action to agent continuation) | 30-60s | <5s |
| Orphaned approvals (no matching session) | ~5% | 0% |
| Agent retries after approval | 100% | 0% |
| User satisfaction with approval UX | Unknown | Measured |

---

## Appendix A: Migration Guide

### For Existing Agents

**Before (manual retry):**
```json
// Tool returns approval_required
{"approval_required": true, "approval_id": "apr-abc123"}

// Agent must retry later:
agent.install({
  ...same args...,
  "approval_ref": "apr-abc123"
})
```

**After (auto-execute):**
```json
// Tool returns pending status
{"pending": true, "approval_id": "apr-abc123"}

// Agent hibernates, wakes with result:
{"tool": "agent.install", "result": {"installed": true, ...}}
```

### For CLI Users

No changes needed - `approve` and `reject` commands work the same.

---

## Appendix B: Related Specs

- `docs/spec-implicit-artifacts-agent-evolution.md` - Implicit artifacts and escalation
- `docs/agent-install-approval-retry.md` - Current approval retry behavior
- `docs/remote-access-approval.md` - Remote access approval specifics

---

## Implementation Status (2026-03-23)

### ✅ Implemented

#### 2. Artifact Deduplication
- **Deterministic artifact IDs**: `compute_deterministic_artifact_id()` in `artifact_store.rs:62-89`
- **Idempotent artifact.build**: Returns `reused: true` for existing artifacts
- **Tests**: `test_artifact_build_and_inspect()`, `test_artifact_immutability()`, `test_artifact_build_validates_entrypoints()`

#### 3.8 Resumption Context
- **Tool result injection**: `inject_resumption_context()` in `tool_call_processor.rs`
- **Fields added**: `resumed`, `original_tool`, `original_args`
- **SKILL.md updates**: All agents now include resumption checklist guidance

#### 5. Approval Visibility (Unified)
- **New workflow event types**: `task.approved`, `task.rejected`
- **Removed SQLite polling**: Chat CLI now uses workflow events only
- **See**: `docs/workflow-orchestration.md` → "Approval Events (New)"

### 📋 Documented In Other Specs

- **Implicit Artifacts**: See `docs/spec-implicit-artifacts-agent-evolution.md`
- **Session Escalation**: See `docs/spec-implicit-artifacts-agent-evolution.md` → "Escalation System"
- **Workflow Events**: See `docs/workflow-orchestration.md` → "Workflow Event Types"

### 🔧 Key Implementation Notes

1. **Resumption Detection**: `history.len() > 2` heuristic in `lifecycle.rs:251`
2. **Approval Events**: Generated in `update_task_run_status()` when `result_summary` contains `"approval_"` prefix
3. **Chat CLI**: Polls workflow events every second, no direct SQLite approval queries
4. **Storage**: Approvals still stored in SQLite for `autonoetic gateway approve` command, but display uses events
