# Design: Checkpoint, Queryable Events & Digest — Autonoetic Session State

## Problem

Autonoetic has 9+ overlapping storage systems for session state. Some data is stored 3 times (workflow events in SQLite → gateway causal chain → timeline.md). Other critical data is inaccessible: causal chain events are JSONL-only (not queryable by agents), full tool results are truncated to 256 chars in the audit trail, and there's no cross-session learning infrastructure.

Current systems:

| System | Location | Purpose | Issue |
|--------|----------|---------|-------|
| Conversation history | Content store (`session_history`) | LLM context reconstruction | Only persisted at hibernation; lost on crash |
| Turn continuation | `.gateway/continuations/{task_id}.json` | Approval gate suspension only | Narrow: only covers approval gates |
| Causal chain (agent) | `{agent}/history/causal_chain.jsonl` | Tamper-evident audit trail | JSONL-only, not queryable by agents |
| Causal chain (gateway) | `.gateway/history/causal_chain.jsonl` | Gateway-level audit | Mirrors workflow events already in SQLite |
| Evidence files | `{agent}/history/evidence/{session}/` | Full tool/LLM payloads | Only written on errors; never read programmatically |
| Session timeline | `.gateway/sessions/{session}/timeline.md` | Human-readable progress | Flat table, no reasoning, no error context |
| Session snapshot | Content store + metadata | Session forking | Rarely used; duplicates history persistence |
| Workflow events | `gateway.db` SQLite | Task orchestration queries | Mirrored to gateway causal chain AND timeline |
| Tier 1 memory | `{agent}/state/` files | Agent working state | Simple, no overlap |
| Tier 2 memory | `.gateway/memory.db` SQLite | Cross-agent durable memory | Separate DB; search is LIKE-only, tags unused in queries |

**Core issues:**
1. No general checkpoint — agent can only respawn from approval gates, not from hibernation, crash, or budget exhaustion.
2. No meaningful session narrative — timeline.md is a flat event table without reasoning, errors, or decisions.
3. Causal chain events are not queryable — agents cannot search their own or other agents' past executions to learn from them.
4. Full code execution results (stdout, stderr, exit_code) are only captured on errors and even then only in evidence files that nothing reads programmatically.
5. Two separate SQLite databases (`gateway.db` and `memory.db`) with no reason to be apart.
6. Memory search ignores tags — the `tags` field exists but `memory.search` only does SQL LIKE on content.
7. No cross-session learning — each session is isolated; there's no mechanism for agents to learn from past errors, successful approaches, or execution patterns.
8. No first-class human interaction primitive — agents can ask in plain text or rely on approval flows, but there is no structured way to ask the user a question, present choices, suspend execution, and resume deterministically with the answer.
9. No operator-grade emergency stop — `workflow.cancel_task` only handles queued or waiting work, not already running tasks or sandbox child processes.

**Design principles:**
- Agents are meant to be increasingly autonomous, self-evolving, and learning (while staying immutable in their SKILL.md definition). Every storage system must be evaluated through this lens: does it help agents learn and improve?
- All structured data must be queryable in SQLite. JSONL files serve as tamper-evident append-only logs, but agents and tools query the DB.
- Evidence files are kept for debugging — they're invaluable during development. Make them configurable (`full`/`errors`/`off`) so production deployments can manage disk usage.
- Code execution traces (especially errors) are first-class data. An agent that failed to compile code last week should be able to recall that error and avoid it.

## Design

### Storage Model

```
.gateway/
├── gateway.db                          # SQLite: ALL queryable state
│   ├── [table] workflow_runs
│   ├── [table] task_runs
│   ├── [table] workflow_events
│   ├── [table] approvals
│   ├── [table] user_interactions     ← NEW: persisted user questions, choices, answers
│   ├── [table] emergency_stops       ← NEW: durable stop requests + audit trail
│   ├── [table] active_executions     ← NEW: running execution leases + kill metadata
│   ├── [table] queued_tasks
│   ├── [table] memories              ← merged from memory.db
│   ├── [table] causal_events         ← NEW: queryable mirror of causal chain
│   └── [table] execution_traces      ← NEW: full code execution results
├── checkpoints/
│   └── {session_id}/
│       └── {turn_id}.checkpoint.json   # Execution snapshot (opaque)
├── content/sha256/                     # Immutable blobs (unchanged)
├── sessions/{session_id}/
│   └── digest.md                       # Live Digest (replaces timeline.md)
└── continuations/{task_id}.json        # Kept: approval-specific (subset of checkpoint)

{agent_dir}/
├── history/
│   ├── causal_chain.jsonl              # Kept: tamper-evident append-only log
│   └── evidence/{session}/             # Kept: full payloads (configurable)
└── state/                              # Kept: Tier 1 working memory
```

**What changes:**

| System | Action | Reason |
|--------|--------|--------|
| Gateway causal chain | **Delete** | All its events are already in `gateway.db` tables (workflow_events, approvals) or go to agent chains. The JSONL was a redundant copy. |
| Session timeline | **Replace** with Live Digest | Same trigger points, richer content, structured for agent consumption |
| Session snapshot | **Subsume** into checkpoint | Checkpoint is a superset; forking reads from checkpoint |
| `memory.db` | **Merge** into `gateway.db` | One DB, one connection pool, one backup target |
| Evidence files | **Keep** | Valuable for debugging. Made configurable: `full` / `errors` / `off` |
| Agent causal chain | **Keep + mirror to DB** | JSONL stays as tamper-evident log. New `causal_events` table in gateway.db provides queryable access to the same data |

**What's new:**

| System | Purpose |
|--------|---------|
| `causal_events` table | Queryable mirror of all causal chain events — agents can search past executions |
| `execution_traces` table | Full code execution results (command, exit_code, stdout, stderr, duration) — not truncated |
| `user_interactions` table | Structured user questions, options, and answers — queryable and resumable |
| `emergency_stops` table | Durable operator stop requests and final stop outcome for audit / CLI visibility |
| `active_executions` table | Running execution leases and kill metadata so emergency stop can target live work |
| `artifact_refs` table | Scoped short refs (`session`/`workflow`/`global`) mapped to canonical artifact digest |
| Checkpoint system | Universal execution snapshots at all yield points |
| Live Digest | Structured real-time narrative with reasoning annotations |
| Post-session digest agent | LLM-powered summarization + memory extraction |

---

### Artifact Identity and Reference Model (LLM-Friendly + Robust)

**Goal:** Keep short IDs for LLM ergonomics while making artifact identity cryptographically strong and safe for cross-agent/cross-network reuse.

#### Canonical identity vs short reference

Artifacts have two identifiers:

1. **Canonical identity (authoritative):**
   - `artifact_digest` = SHA-256 of canonical manifest bytes (full file handles + entrypoints + metadata)
   - Used for dedup, integrity checks, network transfer, and policy decisions
   - Never ambiguous, never scoped

2. **Short reference (LLM-facing):**
   - Example: `ar.wf-9f3c.004.k7p2`
   - Stored as a mapping in SQLite
   - Scoped to `session`, `workflow`, or `global`
   - Intended for agent prompts and tool arguments only

**Rule:** Agents pass short refs in turns; gateway resolves to canonical digest internally.

#### New tables

```sql
CREATE TABLE artifact_refs (
    ref_id         TEXT PRIMARY KEY,      -- short, LLM-friendly (scoped alias)
    scope_type     TEXT NOT NULL,         -- session | workflow | global
    scope_id       TEXT NOT NULL,         -- session_id or workflow_id; "__global__" for global
    artifact_id    TEXT NOT NULL,         -- existing art_* id
    artifact_digest TEXT NOT NULL,        -- canonical SHA-256 manifest digest
    created_by_agent_id TEXT NOT NULL,
    created_at     TEXT NOT NULL,
    expires_at     TEXT,                  -- optional TTL for ephemeral refs
    revoked_at     TEXT
);
CREATE UNIQUE INDEX idx_artifact_ref_scope ON artifact_refs(scope_type, scope_id, ref_id);
CREATE INDEX idx_artifact_ref_artifact ON artifact_refs(artifact_id);
CREATE INDEX idx_artifact_ref_digest ON artifact_refs(artifact_digest);
```

Optional (for secure network exchange):

```sql
CREATE TABLE artifact_shares (
    share_id       TEXT PRIMARY KEY,
    artifact_digest TEXT NOT NULL,
    issued_by_gateway_id TEXT NOT NULL,
    target_gateway_id TEXT,
    audience       TEXT,
    permissions    TEXT,                  -- JSON list
    expires_at     TEXT NOT NULL,
    nonce          TEXT NOT NULL,
    signature      TEXT NOT NULL,
    created_at     TEXT NOT NULL,
    consumed_at    TEXT
);
```

#### Resolution contract (fail-fast)

`artifact.resolve_ref({ref_id, scope_type, scope_id})`:
1. Lookup exact row in `artifact_refs`
2. Reject if expired/revoked
3. Load artifact manifest by `artifact_id`
4. Recompute digest and compare with `artifact_digest`
5. Return canonical identity + manifest metadata

No fallback to other scopes. No fuzzy matching. No default behavior.

#### Output contract for agent-to-agent reuse

When a child task produces reusable output:
- Return one or more `artifact_ref` values (short refs)
- Optionally include top-level summary metadata (`kind`, `entrypoints`, `digest_prefix`)
- Do not require the child to inline all file handles in final natural-language text

This keeps turn payloads compact while preserving deterministic retrieval.

#### Simpler alternatives (choose based on complexity budget)

1. **Minimal hardening (fastest):**
   - Keep current short `art_*` output
   - Add mandatory digest verification before artifact reuse
   - Add collision check on existing artifact ID reuse
   - Pros: smallest change
   - Cons: still weaker namespace semantics for LLM references

2. **Balanced model (recommended):**
   - Canonical digest + scoped short refs (`artifact_refs`)
   - Fail-fast resolution + optional TTL/revocation
   - Pros: robust and still LLM-friendly
   - Cons: moderate schema/tooling work

3. **Strict model (most robust, least ergonomic):**
   - Agents pass only full canonical digests/handles
   - No short refs
   - Pros: simplest semantics
   - Cons: poor LLM usability, higher prompt/token friction

---

### System 1: Checkpoint (Exact Respawn + Reproducibility)

**Purpose:** Save complete execution state so an agent can be respawned at any yield point with identical behavior.

**When written:**
- Hibernation (EndTurn / StopSequence) — agent pauses between turns
- Budget exhaustion — session budget depleted mid-execution
- Approval suspension — already handled by TurnContinuation (which becomes a specialization)
- Max turns reached — loop guard fires
- Crash recovery — checkpoint from last successful turn enables replay

**What's stored:**

```rust
pub struct SessionCheckpoint {
    // --- Execution state (enough to call execute_with_history) ---
    pub history: Vec<Message>,           // full conversation up to this point
    pub turn_counter: u64,               // current turn number
    pub loop_guard_state: LoopGuardState, // failure counts, progress tracking

    // --- Session identity ---
    pub agent_id: String,
    pub session_id: String,
    pub turn_id: String,
    pub workflow_id: Option<String>,
    pub task_id: Option<String>,

    // --- Reproducibility ---
    pub runtime_lock_hash: String,        // SHA-256 of RuntimeLock content
    pub llm_config_snapshot: LlmConfigSnapshot, // model, temperature, max_tokens
    pub tool_registry_version: String,    // hash of registered tool set

    // --- Context ---
    pub yield_reason: YieldReason,        // why execution stopped
    pub content_store_refs: Vec<(String, String)>, // (name, handle) pairs active in session
    pub created_at: String,              // RFC3339

    // --- Pending work (for mid-tool-batch suspension) ---
    pub pending_tool_state: Option<PendingToolState>, // if suspended mid-batch
}

pub enum YieldReason {
    Hibernation,          // EndTurn / StopSequence
    BudgetExhausted,      // session budget depleted
    ApprovalRequired {     // approval gate (overlaps TurnContinuation)
        approval_request_id: String,
    },
    UserInputRequired {    // explicit question / choice for the human
        interaction_id: String,
    },
    EmergencyStop {        // operator circuit breaker; do not auto-resume
        stop_id: String,
    },
    MaxTurnsReached,      // loop guard limit
    ManualStop,           // operator/user interrupt
    Error(String),        // recoverable error
}
```

**Storage:** `.gateway/checkpoints/{session_id}/{turn_id}.checkpoint.json`

**Lifecycle:**
1. Written at each yield point during `execute_with_history`
2. On respawn: load latest checkpoint for session → reconstruct executor state → call `execute_with_history` with checkpoint's history
3. Old checkpoints pruned after N successful turns (configurable, default: keep last 3)
4. On session completion: final checkpoint archived (enables post-mortem replay)

**Relationship to TurnContinuation:** TurnContinuation is kept as a specialized structure for the approval resume path (it has `pending_tool_call`, `remaining_tool_calls`, `completed_tool_results` which are specific to mid-tool-batch suspension). The general checkpoint covers all other yield reasons. When an approval suspension occurs, BOTH a TurnContinuation and a checkpoint are written — the continuation drives the scheduler resume, the checkpoint enables general respawn.

**Relationship to user ask:** Human clarification should use the same checkpoint/resume model as approvals, but without reusing approval-specific state. When an agent calls `user.ask`, the gateway persists a `user_interactions` row, writes a checkpoint with `YieldReason::UserInputRequired`, returns control to the chat surface, and later resumes from checkpoint once the user answers. This makes human clarification a first-class suspension point rather than an ad hoc plain-text convention.

---

### System 1B: Human Interaction and `user.ask`

**Purpose:** Let an agent ask the human a question, optionally present structured choices, suspend execution, and resume deterministically with the answer.

**Why this is separate from approvals:** Approvals are policy gates over privileged actions. Human questions are part of task execution itself: clarifications, tradeoff selection, requirement confirmation, and proposal selection. They need their own schema and resume semantics.

#### `user_interactions` table

```sql
CREATE TABLE user_interactions (
    interaction_id  TEXT PRIMARY KEY,      -- ui-* short id
    session_id      TEXT NOT NULL,
    root_session_id TEXT NOT NULL,
    workflow_id     TEXT,
    task_id         TEXT,
    agent_id        TEXT NOT NULL,
    turn_id         TEXT,
    kind            TEXT NOT NULL,         -- clarification | decision | proposal | confirmation
    question        TEXT NOT NULL,
    context         TEXT,
    options_json    TEXT,                  -- JSON array of {id,label,value}
    allow_freeform  INTEGER NOT NULL,      -- 1 = free text answer allowed
    status          TEXT NOT NULL,         -- pending | answered | cancelled | expired
    answer_option_id TEXT,
    answer_text     TEXT,
    answered_by     TEXT,
    created_at      TEXT NOT NULL,
    answered_at     TEXT,
    expires_at      TEXT
);

CREATE INDEX idx_user_interactions_session ON user_interactions(session_id);
CREATE INDEX idx_user_interactions_root_session ON user_interactions(root_session_id);
CREATE INDEX idx_user_interactions_workflow ON user_interactions(workflow_id);
CREATE INDEX idx_user_interactions_status ON user_interactions(status);
CREATE INDEX idx_user_interactions_agent ON user_interactions(agent_id, created_at);
```

#### Tool contract: `user.ask`

```json
{
  "kind": "clarification",
  "question": "Which output format do you want?",
  "context": "I can produce the report as Markdown, HTML, or JSON. The format affects artifact shape and follow-up tooling.",
  "options": [
    {"id": "md", "label": "Markdown", "value": "markdown"},
    {"id": "html", "label": "HTML", "value": "html"},
    {"id": "json", "label": "JSON", "value": "json"}
  ],
  "allow_freeform": true
}
```

Returns:

```json
{
  "ok": true,
  "interaction_id": "ui-9c1a2f4d",
  "status": "awaiting_user"
}
```

Side effects:
- Insert row into `user_interactions`
- Write checkpoint with `YieldReason::UserInputRequired`
- Emit causal event / workflow event for visibility
- Stop the current turn cleanly

#### Resume contract

When the user answers:
1. Gateway updates `user_interactions.status = 'answered'`
2. Gateway loads the latest checkpoint for the session
3. Gateway reconstructs execution state and injects a synthetic answer record into the resumed turn context
4. Agent continues from the exact suspended point

**Injected answer shape:**

```json
{
  "interaction_id": "ui-9c1a2f4d",
  "kind": "clarification",
  "question": "Which output format do you want?",
  "answer_option_id": "md",
  "answer_text": "markdown"
}
```

#### CLI / chat behavior

- The chat UI renders the question, context, and options
- The user can answer with either an option selection or free text
- `trace` commands can inspect interaction history alongside causal and workflow events

#### Incoming user messages during active workflows

When the user sends a new chat message while child agents are still running, the message should route to the **lead session** for that root session, which is typically the planner. The planner is the coordination point for user intent updates while specialists continue independently in the background.

Expected behavior:
1. User sends a new `event.ingest` chat message for an existing root session
2. Gateway resolves the session's lead binding and delivers the message to the lead agent
3. Running child tasks are **not** interrupted automatically
4. Planner decides how to react:
  - answer immediately without changing delegated work
  - revise the plan and spawn new tasks
  - cancel superseded tasks
  - wait for current tasks and incorporate the new message later
  - escalate to `user.ask` / `workflow.wait` / cancel tools as needed

This keeps the planner as the single authority for orchestration changes while preserving background task durability. It also avoids broadcasting the same user message to every child session.

**Design rule:** direct user messages are planner-managed by default; worker sessions only receive human input explicitly via `user.ask` resume or an explicit targeted routing mechanism.

#### Query examples

- "All pending clarifications in this workflow": `WHERE workflow_id = ? AND status = 'pending'`
- "What decisions did the user make in session X": `WHERE session_id = ? AND kind = 'decision' AND status = 'answered'`
- "How often did this agent need clarification": `WHERE agent_id = ? AND kind = 'clarification'`

---

### System 1C: Emergency Stop / Circuit Breaker

**Purpose:** Let the root session be halted immediately, including all active child work, already running workflow tasks, and sandbox child processes.

**Why this is separate from `workflow.cancel_task`:** `workflow.cancel_task` is a planner-facing orchestration tool for graceful cancellation. Today it only cancels `Pending`, `Runnable`, and `AwaitingApproval` tasks. Emergency stop is a gateway-owned safety control that must also target `Running` work and prevent automatic resume.

**Primary control boundary:** emergency stop is a **root-session** operation first. Internally it fans out to workflow, task, and process scopes, but the user-facing and policy-facing control surface should be `root_session.emergency_stop(root_session_id, reason)`.

**Authorized callers:**
- **User / operator** via chat, CLI, or API against a root session
- **Gateway itself** when a security monitor or policy engine detects a breach, sandbox escape signal, credential exfiltration attempt, or other hard-stop condition
- **Dedicated emergency-manager agent** with an explicit high-privilege capability dedicated to emergency response

Among agents, this dedicated emergency-manager agent is the **only** agent allowed to request emergency stop. No planner, specialist, or ordinary autonomous agent may call it. The gateway remains the sole executor: agents can request, but only the gateway accepts, validates, records, and performs the stop.

The agent path must not reuse ordinary workflow tools. It should use a dedicated policy capability such as `EmergencyStop` so the authority is explicit and tightly reviewable.

**Semantics:**
1. Authorized caller invokes `root_session.emergency_stop` (or an internal gateway equivalent) for a root session
2. Gateway writes a durable stop request row and marks the root workflow/session as stopping
3. Gateway cancels queued work, expires pending human interactions/approvals for the stopped scope, aborts active async task handles, and kills tracked sandbox child processes
4. Each affected task transitions to a terminal aborted/cancelled state
5. Root session receives a terminal checkpoint with `YieldReason::EmergencyStop`; the session does not auto-resume
6. CLI / trace surfaces show whether the stop fully succeeded or was only partially enforced

#### Public contract

```json
{
  "root_session_id": "chat-root-123",
  "reason": "Security breach suspected: attempted secret exfiltration",
  "requested_by_type": "user",
  "requested_by_id": "alice"
}
```

Returns immediately with a durable stop id and current state:

```json
{
  "ok": true,
  "stop_id": "estop-9f3c2a10",
  "root_session_id": "chat-root-123",
  "status": "requested"
}
```

#### Status model

Extend workflow/task lifecycle state so emergency-stop is explicit instead of overloading normal cancellation:

```rust
pub enum WorkflowRunStatus {
  Active,
  WaitingChildren,
  BlockedApproval,
  Resumable,
  EmergencyStopping,
  EmergencyStopped,
  Completed,
  Failed,
  Cancelled,
}

pub enum TaskRunStatus {
  Pending,
  Runnable,
  Running,
  AwaitingApproval,
  Paused,
  Aborting,
  Aborted,
  Succeeded,
  Failed,
  Cancelled,
}
```

`Cancelled` remains the graceful planner/scheduler path. `Aborted` means work was force-stopped by the gateway after it had started or while a stop was in progress.

#### Durable schema

```sql
CREATE TABLE emergency_stops (
  stop_id          TEXT PRIMARY KEY,      -- estop-* short id
  scope_type       TEXT NOT NULL,         -- root_session | workflow | session | task
  scope_id         TEXT NOT NULL,
  root_session_id  TEXT NOT NULL,
  workflow_id      TEXT,
  requested_by_type TEXT NOT NULL,        -- user | gateway | agent
  requested_by_id   TEXT NOT NULL,        -- username | subsystem name | emergency agent id
  reason           TEXT,
  trigger_kind     TEXT NOT NULL,         -- manual | security_policy | automated_response
  mode             TEXT NOT NULL,         -- immediate
  status           TEXT NOT NULL,         -- requested | stopping | stopped | partially_stopped | failed
  requested_at     TEXT NOT NULL,
  completed_at     TEXT,
  details_json     TEXT                   -- JSON summary: killed_pids, aborted_tasks, failures
);

CREATE INDEX idx_emergency_stops_root ON emergency_stops(root_session_id, requested_at);
CREATE INDEX idx_emergency_stops_workflow ON emergency_stops(workflow_id, requested_at);
CREATE INDEX idx_emergency_stops_status ON emergency_stops(status);
CREATE INDEX idx_emergency_stops_requester ON emergency_stops(requested_by_type, requested_by_id, requested_at);

CREATE TABLE active_executions (
  execution_id      TEXT PRIMARY KEY,
  root_session_id   TEXT NOT NULL,
  workflow_id       TEXT,
  task_id           TEXT,
  session_id        TEXT NOT NULL,
  agent_id          TEXT NOT NULL,
  execution_kind    TEXT NOT NULL,        -- root_turn | workflow_task | sandbox_process | middleware
  driver            TEXT,
  pid               INTEGER,
  host_id           TEXT NOT NULL,
  status            TEXT NOT NULL,        -- running | stop_requested | stopped | lost
  started_at        TEXT NOT NULL,
  heartbeat_at      TEXT NOT NULL,
  stop_requested_at TEXT,
  stopped_at        TEXT,
  stop_id           TEXT
);

CREATE INDEX idx_active_executions_root ON active_executions(root_session_id, status);
CREATE INDEX idx_active_executions_workflow ON active_executions(workflow_id, status);
CREATE INDEX idx_active_executions_task ON active_executions(task_id, status);
CREATE INDEX idx_active_executions_session ON active_executions(session_id, status);
```

`active_executions` is not the only kill mechanism. It is the durable ownership ledger used for visibility, restart reconciliation, and best-effort stop propagation. The live gateway still needs an in-memory registry of abort handles / process handles for immediate enforcement.

#### Runtime requirements

- Scheduler registers every running workflow task in an `ActiveExecutionRegistry` with an abort handle before `tokio::spawn`
- Sandbox execution registers child process ownership before blocking on `wait_with_output()`
- Middleware / helper subprocesses register the same way as sandbox processes
- Emergency stop first hits the in-memory registry for immediate abort/kill, then persists final stop outcome in SQLite
- Gateway security monitors can invoke the same root-session stop path directly without going through chat UX or planner mediation
- The dedicated emergency-manager agent must call the same root-session stop path through a dedicated privileged tool/capability, not via generic workflow controls
- Gateway is always the enforcement point: it validates the requester, writes the audit row, and executes the kill / abort sequence itself
- On restart, the gateway scans `active_executions` with stale heartbeats and marks them `lost` or `stopped`; no hidden zombie work

#### Interaction with checkpoints and human interaction

- Pending `user_interactions` in the stopped scope move to `cancelled` with linkage to `stop_id`
- Pending approvals in the stopped scope are marked cancelled/expired and never resume work
- Root session writes a terminal checkpoint using `YieldReason::EmergencyStop { stop_id }`
- A later manual restart must create a fresh workflow or explicitly restore from a non-emergency checkpoint; emergency stop is terminal by default

#### Query examples

- "Show all emergency stops for this root session": `WHERE root_session_id = ? ORDER BY requested_at DESC`
- "Show all security-triggered emergency stops": `WHERE requested_by_type = 'gateway' AND trigger_kind = 'security_policy'`
- "Which tasks were aborted by stop X": `WHERE stop_id = ? AND status IN ('stopped', 'lost')`
- "What work is still believed to be running after a stop": `WHERE root_session_id = ? AND status IN ('running', 'stop_requested')`

---

### System 2: Queryable Event Store (`causal_events` + `execution_traces`)

**Problem being solved:** Causal chain JSONL is append-only and grep-only. Agents cannot ask "what errors did I see last time I touched this file?" or "what approach worked when the coder agent built retry logic?" The data exists but is locked in flat files.

#### 2a: `causal_events` table

Every event written to the agent causal chain is also inserted into `gateway.db`.

```sql
CREATE TABLE causal_events (
    event_id     TEXT PRIMARY KEY,       -- same as causal chain log_id (UUID)
    agent_id     TEXT NOT NULL,
    session_id   TEXT NOT NULL,
    turn_id      TEXT,
    event_seq    INTEGER NOT NULL,
    timestamp    TEXT NOT NULL,          -- RFC3339
    category     TEXT NOT NULL,          -- tool_invoke, llm, lifecycle, memory, ...
    action       TEXT NOT NULL,          -- requested, completed, failure, ...
    status       TEXT NOT NULL,          -- SUCCESS, ERROR, DENIED
    target       TEXT,                   -- tool name, model name, etc.
    payload      TEXT,                   -- full JSON payload (not truncated)
    payload_ref  TEXT,                   -- SHA handle to content store for large payloads
    evidence_ref TEXT,                   -- path to evidence file (when evidence mode enabled)
    reason       TEXT                    -- error/denial reason
);

CREATE INDEX idx_causal_agent_session ON causal_events(agent_id, session_id);
CREATE INDEX idx_causal_category_action ON causal_events(category, action);
CREATE INDEX idx_causal_status ON causal_events(status);
CREATE INDEX idx_causal_target ON causal_events(target);
CREATE INDEX idx_causal_timestamp ON causal_events(timestamp);
```

**Write path:** `SessionTracer::log_event()` writes to BOTH the JSONL (tamper-evident log) and `causal_events` (queryable store). The JSONL remains the integrity source; the DB is the query source.

**Query examples agents can make:**
- "All errors in my last 5 sessions": `WHERE agent_id = ? AND status = 'ERROR' ORDER BY timestamp DESC`
- "All sandbox.exec failures": `WHERE category = 'tool_invoke' AND target = 'sandbox.exec' AND status = 'ERROR'`
- "What happened in session X": `WHERE session_id = ? ORDER BY event_seq`

#### 2b: `execution_traces` table

Full code execution results, not truncated. This is the data agents need to learn from past runs.

```sql
CREATE TABLE execution_traces (
    trace_id     TEXT PRIMARY KEY,       -- UUID
    event_id     TEXT NOT NULL,          -- FK to causal_events (the tool_invoke.completed event)
    agent_id     TEXT NOT NULL,
    session_id   TEXT NOT NULL,
    turn_id      TEXT,
    timestamp    TEXT NOT NULL,
    tool_name    TEXT NOT NULL,          -- sandbox.exec, agent.install, etc.
    command      TEXT,                   -- the command that was executed (for sandbox.exec)
    exit_code    INTEGER,               -- process exit code (null if not applicable)
    stdout       TEXT,                   -- full stdout (not truncated)
    stderr       TEXT,                   -- full stderr (not truncated)
    duration_ms  INTEGER,               -- execution wall time
    success      INTEGER NOT NULL,      -- 1 = ok, 0 = failure
    error_type   TEXT,                   -- classification: compilation, runtime, permission, timeout, ...
    error_summary TEXT,                  -- one-line error description (extractable from stderr)
    approval_required INTEGER DEFAULT 0, -- 1 if this execution was gated by approval
    approval_request_id TEXT,           -- apr-* ID if approval was involved
    arguments    TEXT,                   -- full tool arguments JSON
    result       TEXT                    -- full tool result JSON
);

CREATE INDEX idx_exec_agent_session ON execution_traces(agent_id, session_id);
CREATE INDEX idx_exec_tool ON execution_traces(tool_name);
CREATE INDEX idx_exec_success ON execution_traces(success);
CREATE INDEX idx_exec_error_type ON execution_traces(error_type);
CREATE INDEX idx_exec_command ON execution_traces(command);
```

**Write path:** In `tool_call_processor.rs`, after every tool execution (not just errors), insert a row. For `sandbox.exec` results, parse the JSON to extract `exit_code`, `stdout`, `stderr`. For other tools, store the full result.

**Why a separate table from `causal_events`?** Execution traces have structured fields (exit_code, stdout, stderr, duration_ms, error_type) that enable efficient queries. Storing these as parsed columns rather than buried in a JSON payload blob means agents can query: "show me all compilations that failed with exit_code 1 in the last week" without parsing JSON in SQL.

**Query examples for agent learning:**
- "All compilation errors I've seen": `WHERE error_type = 'compilation' AND success = 0`
- "What commands worked for this task pattern": `WHERE tool_name = 'sandbox.exec' AND success = 1 AND command LIKE '%pytest%'`
- "Errors involving this file": `WHERE stderr LIKE '%src/http/client.rs%' AND success = 0`
- "Average execution time for test suites": `SELECT AVG(duration_ms) FROM execution_traces WHERE command LIKE '%test%' AND success = 1`

---

### System 3: Live Digest (Real-Time Session Narrative)

**Purpose:** Progressive, human-readable, agent-consumable narrative built during session execution. Replaces `timeline.md`.

**Format:** Structured Markdown with typed sections, appended turn by turn.

**Example:**

```markdown
# Session Digest: {session_id}
Agent: {agent_id} | Started: {timestamp}
Task: {initial_user_message_preview}

---

## Turn 1 — {timestamp}
**Action:** Called `sandbox.exec` with `python3 tests/run_all.py`
**Result:** 12 tests passed, 1 failed (test_retry_backoff: timeout after 5s)
**Reasoning:** Running full test suite first to establish baseline before changes.

## Turn 2 — {timestamp}
**Action:** Read `src/http/client.rs` (lines 220-280)
**Observation:** Existing retry stub at line 234, no backoff logic, fixed 1s delay.
**Decision:** Implement exponential backoff with jitter (user requirement).
  - Alternative considered: fixed delay with increased timeout — rejected (fragile on slow networks).

## Turn 3 — {timestamp}
**Action:** Edited `src/http/client.rs` — added exponential backoff (lines 234-267)
**Error:** Compilation failed — `Future + Send` bound missing on async trait method.
  - Root cause: `async fn retry()` in trait requires explicit `Send` bound in this codebase.
  - Fix: Added `+ Send` to trait bound on line 89.
**Artifact:** Modified `src/http/client.rs` (33 lines changed)

## Turn 4 — {timestamp}
**Action:** Called `sandbox.exec` with `python3 tests/run_all.py`
**Result:** 13 tests passed, 0 failed. ✓

---

## Summary
**Outcome:** Completed
**Turns:** 4 | **Tools:** 3 calls | **Errors:** 1 (recovered)
**Artifacts:** src/http/client.rs (art_8f2a)
**Key Decision:** Exponential backoff over fixed delay (user requirement).
**Lesson:** Async trait methods in this codebase require explicit `+ Send` bound.
```

**How it's built:**

The Live Digest is NOT generated by the agent itself (that would add unreliable LLM interpretation during execution). Instead, it's built by the **gateway** from structured signals:

1. **Tool results** → formatted from tool name + arguments + result (existing data in `process_tool_calls`)
2. **LLM response metadata** → stop reason, token usage (existing from `StopReason`)
3. **Errors** → tool errors already classified by `ToolError` (permission, validation, resource, etc.)
4. **Agent annotations** → NEW: a lightweight `digest.annotate` tool that lets the agent emit reasoning/decision notes without affecting the LLM conversation history

The `digest.annotate` tool is key: it's how reasoning gets captured. The agent calls it to explain decisions, note alternatives, or flag open items. It's cheap (no LLM call, just appends to digest) and optional (digest still works without it, just with less reasoning context).

```rust
pub struct DigestAnnotateTool;

impl NativeTool for DigestAnnotateTool {
    fn name(&self) -> &'static str { "digest.annotate" }

    // Always available — no capability requirement
    fn is_available(&self, _manifest: &AgentManifest) -> bool { true }

    // Arguments: { "type": "reasoning|decision|observation|lesson", "content": "..." }
    // Returns: { "ok": true }
    // Side effect: appends structured annotation to live digest
}
```

**Storage:** `.gateway/sessions/{session_id}/digest.md`

**Who reads it:**
- Humans: tail during session, review after
- Post-session digest agent: primary input (avoids reading full 400-message history)
- Agents: can read their own or parent session digests for context

---

### System 4: Evidence Files (Kept, Configurable)

**No longer deleted.** Evidence files are valuable for debugging — full unredacted LLM responses, tool arguments, execution results in individual JSON files that can be inspected with any tool.

**Change:** Make evidence mode a first-class config option instead of just an env var.

```yaml
# In gateway config
evidence:
  mode: "full"        # "full" | "errors" | "off"
  # full:   all tool results, all LLM completions (development default)
  # errors: only failures, approval gates, non-zero exit codes (production recommended)
  # off:    no evidence files (causal_events DB still captures everything)
```

**Key invariant:** Even when evidence mode is `off`, the `causal_events` and `execution_traces` tables in `gateway.db` still capture full data. Evidence files are a convenience layer for filesystem-based debugging, not the source of truth.

---

### System 5: Post-Session Digest Agent

**Purpose:** After session completion, produce a compressed narrative and extract queryable memories for agent learning.

**Trigger:** Session end event (or on-demand via CLI).

**Input:** Live Digest + execution_traces summary (errors, patterns) + causal_events summary.

**Output:**
1. **Session narrative** — stored in content store, registered as `session_digest` in session manifest. A refined, compressed version of the Live Digest with cross-references.
2. **Extracted memories** — written to `memories` table in `gateway.db` with typed tags:
   - `type:error_lesson` — what went wrong, root cause, fix applied. Includes: tool, command/file, error output, resolution.
   - `type:decision` — choices made and rationale. Includes: alternatives considered, constraints that drove the choice.
   - `type:approach` — strategies that worked (or didn't). Includes: task pattern, outcome, conditions.
   - `type:fact` — discovered facts about the codebase or environment. Includes: file, API, behavior observed.
   - `type:open_item` — unresolved issues for future sessions. Includes: description, why deferred, severity.

**Cost control:**
- Input is the Live Digest (typically 1-5KB), not the full conversation history (could be 100KB+)
- Single LLM call per session
- Can be disabled via config (`digest_agent.enabled = false`)
- Skipped for trivial sessions (< 2 turns, no errors, no decisions)

**Implementation:** A built-in agent with a fixed SKILL.md, invoked by the scheduler at session end. No special runtime.

---

### System 6: Enhanced Memory with Tag-Based Queries

**Problem:** Current `memory.search` only does SQL LIKE on `content`. Tags exist in the schema but are never used in queries. For agent learning, tag-based queries are essential: "all error lessons for agent X" or "all decisions about retry logic."

**Changes to memory search:**

```sql
-- Current query (broken for learning):
SELECT * FROM memories WHERE scope = ?1 AND content LIKE ?2

-- New query (supports tag filters):
SELECT * FROM memories
WHERE scope = ?1
  AND (?2 IS NULL OR content LIKE ?2)
  AND (?3 IS NULL OR EXISTS (
    SELECT 1 FROM json_each(tags) WHERE json_each.value LIKE ?3
  ))
ORDER BY updated_at DESC
```

**New tool: `memory.search_by_tags`:**
```json
{
  "scope": "agent",
  "tags": ["type:error_lesson", "domain:http"],
  "text": "retry",
  "limit": 10
}
```

Returns memories matching ALL specified tags AND optional text search. This is what agents use for learning: "show me error lessons related to HTTP retries."

**New tool: `execution.search`:**

Direct query interface to `execution_traces` table. Agents can search past code executions.

```json
{
  "tool_name": "sandbox.exec",
  "success": false,
  "error_type": "compilation",
  "command_pattern": "%client.rs%",
  "limit": 5
}
```

Returns structured execution results that the agent can learn from without parsing free text.

---

### Unified Gateway DB

**Change:** Merge `memory.db` into `gateway.db`. All tables in one database.

**Final `gateway.db` schema:**

```
gateway.db
├── workflow_runs        (existing)
├── task_runs            (existing)
├── workflow_events      (existing)
├── approvals            (existing)
├── user_interactions    (NEW: structured user questions + answers)
├── emergency_stops      (NEW: durable stop requests + outcomes)
├── active_executions    (NEW: running execution leases + kill metadata)
├── queued_tasks         (existing)
├── memories             (merged from memory.db, enhanced tag queries)
├── causal_events        (NEW: queryable mirror of causal chain JSONL)
├── execution_traces     (NEW: full code execution results)
├── artifact_refs        (NEW: short scoped refs -> canonical digest)
└── artifact_shares      (optional: signed cross-gateway share envelopes)
```

---

## Interaction Between Systems

```
During Session:
                                                    ┌─────────────────┐
  execute_with_history() ──turn loop──►             │  Live Digest     │
       │                                            │  (digest.md)     │
       │  tool results, errors, LLM metadata        │  progressive MD  │
       │  agent annotations (digest.annotate)        └─────────────────┘
       │
       ├── at each yield point ──────────────►      ┌─────────────────┐
       │                                            │  Checkpoint      │
       │                                            │  (opaque JSON)   │
       │                                            └─────────────────┘
      │
      ├── user.ask ─────────────────────────►      ┌─────────────────┐
      │                                            │ user_interactions│
      │                                            │ gateway.db       │
      │                                            └─────────────────┘
      │                                                     │
      │                                              answer selected
      │                                                     │
      │                                                     ▼
      │                                            ┌─────────────────┐
      │                                            │ Resume from      │
      │                                            │ checkpoint       │
      │                                            └─────────────────┘
       │
        ├── emergency stop ──────────────────►      ┌─────────────────┐
        │                                            │ emergency_stops │
        │                                            │ active_execs    │
        │                                            └─────────────────┘
        │                                                     │
        │                                   abort async tasks / kill pids
        │                                                     │
        │                                                     ▼
        │                                            ┌─────────────────┐
        │                                            │ terminal        │
        │                                            │ checkpoint      │
        │                                            └─────────────────┘
        │
       ├── per event ────────────────────────►      ┌─────────────────┐
       │                                            │  Causal Chain    │
       │                                            │  (JSONL, hashed) │
       │                                            └─────────────────┘
       │                                                   │
       │                                            (dual write)
       │                                                   │
       │                                                   ▼
       ├── per event ────────────────────────►      ┌─────────────────┐
       │                                            │  gateway.db      │
       │                                            │  causal_events   │
       │                                            └─────────────────┘
       │
       └── per tool execution ───────────────►      ┌─────────────────┐
                                                    │  gateway.db      │
           full stdout/stderr/exit_code             │  execution_traces│
           (every execution, not just errors)        └─────────────────┘

           (when evidence mode = full|errors)  ──►  ┌─────────────────┐
                                                    │  Evidence files  │
                                                    │  (JSON, debug)   │
                                                    └─────────────────┘

After Session:
  ┌──────────────┐     ┌──────────────┐     ┌──────────────────────────┐
  │ Live Digest  │────►│ Digest Agent │────►│ Narrative (content store) │
  │ Exec traces  │     │ (LLM call)   │     │ Memories  (gateway.db)   │
  │ Causal summ. │     └──────────────┘     └──────────────────────────┘
  └──────────────┘

On Respawn:
  ┌──────────────┐
  │ Checkpoint   │────► reconstruct executor ──► execute_with_history()
  └──────────────┘

On User Answer:
  ┌──────────────────┐
  │ user_interactions│────► update answer ──► load checkpoint ──► resume turn
  └──────────────────┘

Agent Learning (cross-session):
  ┌──────────────┐
  │ gateway.db   │◄──── execution.search (past errors, patterns)
  │              │◄──── memory.search_by_tags (lessons, decisions)
  │              │◄──── causal_events queries (what happened when)
  └──────────────┘
```

---

## What Gets Deleted

| System | Action | Reason |
|--------|--------|--------|
| Gateway causal chain (`.gateway/history/`) | **Delete** | All data now in agent chains + gateway.db. This was a redundant JSONL copy of SQLite data. |
| Session timeline (`timeline.md`) | **Replace** with Live Digest | Same trigger points, richer content |
| Session snapshot system | **Subsume** into checkpoint | Checkpoint is a superset; forking reads from checkpoint |
| `memory.db` | **Merge** into `gateway.db` | One DB, one connection pool |

**Kept:**
- Agent causal chain JSONL — tamper-evident, append-only (integrity guarantee that SQLite doesn't provide)
- Evidence files — configurable, valuable for debugging
- Content store — unchanged
- Tier 1 memory — simple, useful

---

## Implementation Plan

### Phase 1: Queryable Event Store
Make causal chain data and execution results queryable. This is foundation for agent learning.

- [x] **1.1** Add `causal_events` table to `GatewayStore::open()` in `gateway_store.rs`. Schema as defined above with indexes on (agent_id, session_id), (category, action), (status), (target), (timestamp).
- [x] **1.2** Add `execution_traces` table to `GatewayStore::open()`. Schema as defined above with indexes on (agent_id, session_id), (tool_name), (success), (error_type), (command).
- [x] **1.3** Dual-write in `SessionTracer`: every `log_event()` call writes to BOTH the JSONL causal chain AND inserts into `causal_events` table. Pass `GatewayStore` reference to `SessionTracer`.
- [x] **1.4** Write execution traces in `tool_call_processor.rs`: after every tool execution (not just errors), insert into `execution_traces`. For `sandbox.exec`, parse result JSON to extract `exit_code`, `stdout`, `stderr`. For other tools, store full result. Classify `error_type` from `ToolError` categories (compilation, runtime, permission, timeout, validation, resource).
- [x] **1.5** Implement `execution.search` native tool in `tools.rs`. Arguments: `{ tool_name, success, error_type, command_pattern, agent_id, limit }`. Queries `execution_traces` table. Returns structured results. Available to all agents (no capability gate — agents should learn from all visible executions).
- [x] **1.6** Remove gateway causal chain (`.gateway/history/causal_chain.jsonl`). Stop calling `init_gateway_causal_logger()` and `log_gateway_causal_event()` in `execution.rs`. Gateway-level events that matter (agent spawn, approvals) are already in `gateway.db` tables or can be written to agent causal chains.
- [x] **1.7** Update `trace session` CLI to query `causal_events` table instead of reading JSONL files. Add `--agent` filter. Show evidence_ref when available.
- [x] **1.8** Make evidence mode a config option (not just env var). Support `full`/`errors`/`off`. Default: `full` in dev, recommend `errors` in production.
- [x] **1.9** Unit test: dual-write produces identical event data in JSONL and causal_events table.
- [x] **1.10** Unit test: execution_traces captures full stdout/stderr for both successful and failed sandbox.exec calls.
- [x] **1.11** Integration test: agent runs sandbox.exec → fails → execution_traces has full error → agent uses `execution.search` to find the error → gets structured result.
- [x] **1.12** Add `artifact_refs` table in `GatewayStore` (schema above). Scoped short ref mapping (`session`/`workflow`/`global`) to canonical artifact identity, plus store APIs (`create`/`resolve`/`revoke`/`list`) and unit coverage for migration idempotency, strict scope isolation, and expiry/revocation filtering.
- [x] **1.13** Update `artifact.build` output to include both `artifact_id` and canonical `artifact_digest` (additive alias for existing `digest`); mint scoped short ref in `artifact_refs` when `GatewayStore` is wired (workflow scope when root session is workflow-indexed, else session scope). New ref rows are minted only on first materialization (`reused: false`).
- [x] **1.14** Implement `artifact.resolve_ref` tool with strict scope lookup + digest revalidation. Hard-fail on missing/expired/revoked refs (no fallback).
- [x] **1.15** Add collision safety in artifact reuse path: if an existing `artifact_id` is found but on-disk manifest identity (sorted name/handle pairs + entrypoints) does not match the requested build, fail loudly (no silent reuse).
- [x] **1.16** Integration test: child task returns short `artifact_ref`; parent resolves and inspects artifact successfully without file-handle inlining.
- [ ] **1.17** (Optional) Add `artifact_shares` table + signed share envelope workflow for cross-gateway artifact transfer.

#### Phase 1A: Concrete PR Slices (Artifact Refs) — ✅ Completed

This breaks `1.12`–`1.17` into independently shippable PRs with explicit file touch points.

**PR-A (schema + store methods, no behavior change): ✅ Completed**
- Files:
  - `autonoetic-gateway/src/scheduler/gateway_store.rs`
  - `autonoetic-types/src/artifact.rs` (add typed DTOs for refs/shares)
- Add migrations:
  - `artifact_refs` table + indexes
  - optional `artifact_shares` table + indexes
- Add `GatewayStore` methods:
  - `create_artifact_ref(...)`
  - `resolve_artifact_ref(scope_type, scope_id, ref_id)`
  - `revoke_artifact_ref(...)`
  - `list_artifact_refs_for_scope(...)`
  - (optional) `create_artifact_share(...)`, `consume_artifact_share(...)`
- Tests:
  - Unit test for migration idempotency
  - Unit test for strict scope resolution (no cross-scope fallback)
  - Unit test for expiry/revocation behavior

**PR-B (artifact build path hardening + ref minting): ✅ Completed**
- Files:
  - `autonoetic-gateway/src/artifact_store.rs`
  - `autonoetic-gateway/src/runtime/tools.rs` (`artifact.build`)
- Changes:
  - Ensure dedup/reuse verifies canonical digest match before returning existing artifact
  - Keep existing `artifact_id` response, add:
    - `artifact_digest`
    - `artifact_ref` (scoped short ref)
  - Ref scope selection:
    - if workflow context present -> `workflow`
    - else -> `session`
- Tests:
  - Unit test: same manifest -> same digest, safe reuse
  - Unit test: synthetic ID collision path fails hard on digest mismatch
  - Integration test: `artifact.build` returns both digest + short ref

**PR-C (resolution tool + agent contract): ✅ Completed**
- Files:
  - `autonoetic-gateway/src/runtime/tools.rs` (new `artifact.resolve_ref`)
  - `autonoetic-gateway/tests/artifact_build_ref_integration.rs` (integration tests)
- Tool contract:
  - Input: `{ ref_id, scope_type, scope_id }`
  - Output: `{ ok, artifact_id, artifact_digest, files, entrypoints, created_at, builder_session_id, ref_created_at, ref_created_by }`
  - Hard fail on: missing ref, wrong scope, expired, revoked, digest mismatch
- Tests:
  - Integration test: child emits `artifact_ref`, parent resolves + inspects without raw handles ✅
  - Integration test: wrong scope fails deterministically ✅
  - Integration test: missing ref fails ✅
  - Integration test: expired ref fails ✅
  - Integration test: revoked ref fails ✅

**PR-D (optional network share envelope):**
- Files:
  - `autonoetic-gateway/src/runtime/tools.rs` (e.g. `artifact.share`, `artifact.import_share`)
  - `autonoetic-gateway/src/scheduler/gateway_store.rs`
  - docs for share security model
- Behavior:
  - Create signed envelope bound to canonical digest + expiry + nonce + audience
  - Verify signature and digest on import
  - Persist audit events for issue/consume/reject
- Tests:
  - Signature verification fail path
  - Expired envelope fail path
  - Digest mismatch fail path

#### Rollout / Compatibility Rules

1. `artifact.build` continues returning existing fields (`artifact_id`, `files`, etc.) for backward compatibility.
2. New fields (`artifact_digest`, `artifact_ref`) are additive in the first rollout.
3. Prompt/tooling migration can switch agent playbooks to prefer `artifact_ref` after PR-C lands.
4. No destructive migration for existing artifacts: refs are minted lazily on future `artifact.build` calls (or optional one-time backfill job).

#### Simpler fallback (if schedule pressure is high)

If we need a faster path than scoped refs:
- Keep current `artifact_id` UX
- Add mandatory digest verification in artifact reuse
- Add `artifact_digest` to all tool outputs and require downstream checks
- Defer scoped refs and share envelopes to later phase

### Phase 2: Checkpoint System
Generalize `TurnContinuation` into a universal `SessionCheckpoint` at all yield points.

- [x] **2.1** Define `SessionCheckpoint`, `YieldReason`, `LlmConfigSnapshot` structs in a new `autonoetic-gateway/src/runtime/checkpoint.rs`. Include all fields needed for exact respawn.
- [x] **2.2** Add `save_checkpoint()` and `load_latest_checkpoint()` functions. Storage: `.gateway/checkpoints/{session_id}/{turn_id}.checkpoint.json`. Include pruning logic (keep last N, default 3).
- [x] **2.3** Write checkpoint at hibernation yield points in `lifecycle.rs`. After `StopReason::EndTurn` / `StopReason::StopSequence` handling, call `save_checkpoint()` with `YieldReason::Hibernation`.
- [x] **2.4** Write checkpoint at budget exhaustion. Add `save_checkpoint()` with `YieldReason::BudgetExhausted` before returning.
- [x] **2.5** Write checkpoint at max turns (loop guard). Add `save_checkpoint()` with `YieldReason::MaxTurnsReached`.
- [x] **2.6** Add `LlmConfigSnapshot` capture at session start. Store `runtime_lock_hash` (SHA-256 of `runtime.lock` content).
- [x] **2.7** Implement `respawn_from_checkpoint()` in `execution.rs`: load checkpoint → reconstruct `AgentExecutor` state → call `execute_with_history` with checkpoint history. Wire into `spawn_agent_once` as alternative to fresh start.
- [x] **2.8** Subsume session snapshot into checkpoint: modify `SessionFork::fork()` to read from checkpoint. Keep `session_snapshot.rs` for backward compatibility; add `fork_from_checkpoint()` method.
- [x] **2.9** Integration test: agent runs 3 turns → hibernates → checkpoint saved → new executor loads checkpoint → agent continues from turn 4 with correct history and loop guard state.
- [x] **2.10** Integration test: agent hits budget limit → checkpoint saved → respawn with increased budget → agent continues.

### Phase 2B: Human Interaction Suspension
Add a first-class `user.ask` tool that suspends execution and resumes from checkpoint with the human's answer.

- [ ] **2B.1** Add `user_interactions` table to `GatewayStore::open()` in `gateway_store.rs`. Schema as defined above with indexes on session/root_session/workflow/status.
- [ ] **2B.2** Extend `YieldReason` with `UserInputRequired { interaction_id }` in `checkpoint.rs`.
- [ ] **2B.3** Implement `user.ask` native tool in `tools.rs`. Arguments: `{ kind, question, context, options, allow_freeform, expires_at? }`. Always available.
- [ ] **2B.4** On `user.ask`, persist interaction row, emit a causal event, save checkpoint, and stop the current turn cleanly.
- [ ] **2B.5** Add gateway APIs / CLI plumbing to answer an interaction by `interaction_id` with either `answer_option_id` or `answer_text`.
- [ ] **2B.6** Implement `resume_from_user_interaction()` in `execution.rs`: load checkpoint, inject the recorded answer into resumed state, and continue execution.
- [ ] **2B.7** Wire chat UI rendering so questions with options are shown as structured prompts instead of plain assistant text only.
- [ ] **2B.8** Extend `trace` commands to display user interactions alongside workflow and causal history.
- [ ] **2B.9** Integration test: agent calls `user.ask` → session suspends → user answers → agent resumes from checkpoint with preserved loop guard and history.
- [ ] **2B.10** Integration test: option-based answer resumes with selected option id and canonical value.
- [ ] **2B.11** Integration test: freeform answer resumes with raw user text and is captured in digest + causal history.
- [ ] **2B.12** Document and enforce incoming user message routing during active workflows: default route to lead/planner session; do not auto-interrupt running child tasks.
- [ ] **2B.13** Integration test: user sends a new message while async child tasks are running → planner receives it → child tasks continue unless planner cancels them explicitly.

### Phase 2C: Emergency Stop / Circuit Breaker
Add a true root-session emergency stop path for running workflows, sessions, and sandbox child processes.

- [ ] **2C.1** Extend `WorkflowRunStatus` and `TaskRunStatus` in `autonoetic-types/src/workflow.rs` with `EmergencyStopping`, `EmergencyStopped`, `Aborting`, and `Aborted`. Preserve serde compatibility for existing states.
- [ ] **2C.2** Add `emergency_stops` and `active_executions` tables to `GatewayStore::open()` in `gateway_store.rs`, with store APIs for create/update/list and stale-heartbeat reconciliation.
- [ ] **2C.3** Introduce an in-memory `ActiveExecutionRegistry` in the gateway runtime to track `tokio` abort handles and sandbox/middleware process kill handles keyed by root session / workflow / task / session.
- [ ] **2C.4** Register workflow child tasks before `tokio::spawn` in `scheduler.rs`, and unregister them on normal completion, failure, approval suspension, or abort.
- [ ] **2C.5** Register sandbox child process ownership before `wait_with_output()` in both lifecycle and native-tool execution paths so emergency stop can kill already-running child processes.
- [ ] **2C.6** Implement `root_session.emergency_stop` plus CLI/API/chat plumbing. Behavior: persist stop request, mark the root workflow/session `EmergencyStopping`, cancel queued tasks, abort live tasks/processes, cancel pending approvals/interactions in scope, then finalize status.
- [ ] **2C.6a** Add an internal gateway self-protection entrypoint that invokes the same stop pipeline when security policy detects a hard-stop breach.
- [ ] **2C.6b** Reserve a dedicated privileged capability/tool path for the emergency-manager agent as the only agent-authorized requester; do not expose emergency stop through ordinary workflow delegation tools.
- [ ] **2C.7** Write a terminal checkpoint with `YieldReason::EmergencyStop { stop_id }` and prevent auto-resume from that checkpoint.
- [ ] **2C.8** Surface emergency-stop state in `trace` / workflow inspection commands, including partial-stop failures and any `lost` active executions after restart.
- [ ] **2C.9** Integration test: root workflow with two running async children receives emergency stop → queued work cancelled, running tasks aborted, workflow ends `EmergencyStopped`.
- [ ] **2C.10** Integration test: sandbox child process running under `wait_with_output()` receives emergency stop → process is killed and task ends `Aborted`.
- [ ] **2C.11** Integration test: emergency stop during pending approval or `user.ask` interaction cancels the pending gate and does not allow resume.
- [ ] **2C.12** Restart test: gateway crashes after stop requested but before completion → stale `active_executions` reconciled on startup and stop finishes as `stopped` or `partially_stopped` with audit details.
- [ ] **2C.13** Authorization test: user/operator, gateway security subsystem, and the dedicated emergency-manager agent path are accepted; all other agents are denied.

### Phase 3: Live Digest
Replace `timeline.md` with a richer real-time narrative.

- [ ] **3.1** Create `autonoetic-gateway/src/runtime/live_digest.rs` with `LiveDigestWriter`. Methods: `start_session()`, `start_turn()`, `record_action()`, `record_result()`, `record_error()`, `record_annotation()`, `end_turn()`, `write_summary()`. Output: structured Markdown.
- [ ] **3.2** Implement `digest.annotate` native tool in `tools.rs`. Arguments: `{ "type": "reasoning|decision|observation|lesson", "content": "..." }`. Appends to live digest. Always available.
- [ ] **3.3** Wire `LiveDigestWriter` into `execute_with_history` turn loop. Replace `SessionTimeline` calls with `LiveDigestWriter` calls.
- [ ] **3.4** Add tool result formatting: for `sandbox.exec`, extract exit_code, stdout preview, stderr preview. For errors, extract error_type and message. For artifacts, extract ID and file list. For `user.ask`, record the question, options, and final answer.
- [ ] **3.5** Add turn summary and session summary blocks.
- [ ] **3.6** Remove `session_timeline.rs` and all `SessionTimeline` references.
- [ ] **3.7** Update agent system prompts to document `digest.annotate` tool.
- [ ] **3.8** Integration test: agent runs session → digest.md has structured entries with actions, results, errors, and annotations.

### Phase 4: Unified Gateway DB + Enhanced Memory
Merge `memory.db`, add tag-based queries.

- [ ] **4.1** Add `memories` table to `GatewayStore::open()`. Same schema as current `memory.db`.
- [ ] **4.2** Update `memory.rs` to use `GatewayStore` reference instead of own SQLite connection.
- [ ] **4.3** Write migration: copy `memory.db` rows to `gateway.db`, rename old file.
- [ ] **4.4** Implement `memory.search_by_tags` tool. Arguments: `{ scope, tags, text, limit }`. Queries with JSON tag matching. Available to all agents with memory capability.
- [ ] **4.5** Add tag index: `CREATE INDEX idx_memories_tags ON memories(tags)` for JSON extraction queries.
- [ ] **4.6** Remove `memory.db` creation logic.
- [ ] **4.7** Unit test: memory search by tags returns correct results.
- [ ] **4.8** Integration test: agent stores tagged memory → second agent with visibility can search by tag → finds it.

### Phase 5: Post-Session Digest Agent
LLM-powered summarization and memory extraction.

- [ ] **5.1** Create built-in digest agent: `agents/digest/SKILL.md`. Input: live digest + execution error summary (from `execution_traces`). Output: structured JSON with `narrative` and `memories` array (each memory has `type`, `content`, `tags`, `confidence`).
- [ ] **5.2** Implement `trigger_digest_agent()` in scheduler: called at session end. Reads live digest. Queries `execution_traces` for errors in this session. Spawns digest agent. Stores narrative in content store. Writes memories to `gateway.db`.
- [ ] **5.3** Add session-end trigger. Guard: skip if session < 2 turns or config disabled.
- [ ] **5.4** Implement `digest.query` tool: queries memories by tags + searches session narratives by content handle. Combines structured memory recall with narrative context.
- [ ] **5.5** Add `trace digest <session_id>` CLI command.
- [ ] **5.6** Integration test: session completes → digest agent produces narrative and memories → memories queryable → narrative viewable via CLI.

### Phase 6: Cleanup & Documentation

- [ ] **6.1** Remove `session_snapshot.rs` if fully replaced by checkpoint.
- [ ] **6.2** Audit all `evidence_ref` reads — ensure they work with both filesystem paths (legacy) and content store handles (new).
- [ ] **6.3** Add `execution_traces` pruning policy (keep last N days, configurable).
- [ ] **6.4** Add `causal_events` pruning policy (keep last N days or archive to cold storage).
- [ ] **6.5** Update `docs/ARCHITECTURE.md` with new storage model.
- [ ] **6.6** Update `CLAUDE.md` with checkpoint, event store, and digest architecture.
- [ ] **6.7** Write `docs/agent-learning.md`: how agents use `execution.search`, `memory.search_by_tags`, and `digest.query` to learn from past sessions.

#### Phase 6 Optional: Checkpoint Reproducibility Enhancements (Mid-term)

These are optional improvements to make checkpoint respawn more strictly reproducible. Not required for core functionality.

- [ ] **6.O1** Populate `tool_invocations_consumed` in checkpoint — track tool call counts per session for accurate budget restoration on respawn.
- [ ] **6.O2** Populate `content_store_refs` in checkpoint — capture active (name, handle) pairs so respawn can restore content store references.
- [ ] **6.O3** Compute `tool_registry_version` hash — detect tool registry changes between checkpoint save and respawn; warn or fail if mismatch.

---

## Agent Learning: How It All Comes Together

An agent (e.g., the coder) starting a new session has access to:

1. **`execution.search`** — "Have I seen this error before?"
   ```json
   { "error_type": "compilation", "command_pattern": "%client.rs%", "success": false, "limit": 5 }
   ```
   Returns: 3 past compilation failures involving client.rs, with full stderr and the commands that produced them.

2. **`memory.search_by_tags`** — "What lessons did previous sessions learn?"
   ```json
   { "tags": ["type:error_lesson", "domain:http"], "limit": 10 }
   ```
   Returns: "Async trait methods require explicit `+ Send` bound" (confidence: 0.95, from session abc123).

3. **`digest.query`** — "What approaches have been tried for this kind of task?"
   ```json
   { "tags": ["type:approach"], "text": "retry backoff" }
   ```
   Returns: approach memories + links to full session narratives for context.

4. **Live Digest** from parent session — the planner's digest shows what it asked the coder to do and what constraints apply.

5. **Checkpoint** — if this is a respawn, the agent has its exact prior state and can continue without any loss.

The agent doesn't need to be told to use these — the system prompts document them, and an agent that encounters an error will naturally check past execution traces. Over time, the memories table accumulates a growing body of lessons, facts, and approaches that any agent can query. The agents stay immutable (SKILL.md doesn't change), but their effective knowledge grows through the shared memory layer.

---

## Risk Assessment

| Risk | Mitigation |
|---|---|
| `execution_traces` table grows large (full stdout per execution) | Pruning policy (Phase 6.3); stdout/stderr capped at 64KB per field |
| `causal_events` duplicates causal chain JSONL | JSONL is integrity source (hash chain), DB is query source. Different purposes. |
| `digest.annotate` adds noise to agent prompts | Zero-cost: no LLM tokens in result, simple `{"ok": true}` return |
| Post-session digest agent LLM cost | Input is live digest (1-5KB); skip trivial sessions; configurable |
| Evidence files consume disk in production | Configurable: `full`/`errors`/`off`. Recommend `errors` for production. |
| Checkpoint files grow large | Prune old checkpoints (keep last 3); history capped at 400 messages |
| Agents may over-ask the user | Add `kind`, `expires_at`, and future rate-limit / policy controls; surface pending interactions so planners can batch or avoid redundant asks |
| Emergency stop creates false confidence if live handles are not tracked | Make `active_executions` + in-memory registry mandatory for stop-on-running-work; report `partially_stopped` or `lost` explicitly instead of pretending success |
| Emergency stop authority becomes too broad | Make root-session stop the single public surface, record requester type/id, allow only the dedicated emergency-manager agent on the agent side, and keep execution in the gateway |
| Memory table grows unbounded | TTL/archival policy (Phase 6); confidence scores enable aging |
| Agents overwhelmed by too many search results | All search tools have `limit` parameter; results sorted by recency |
| Short artifact IDs collide or are confused across sessions | Use scoped short refs (`artifact_refs`) mapped to canonical digest; enforce strict scope lookup and digest verification |
| Cross-gateway artifact tampering risk | Share signed envelopes tied to canonical digest + expiry + nonce; verify before import |

---

## Success Criteria

1. **Exact respawn:** Agent checkpointed at turn N can be respawned and produces identical turn N+1 output (given same LLM seed).
2. **Crash recovery:** Agent that crashes mid-session can be resumed from last checkpoint with no visible discontinuity.
3. **Queryable history:** Agents can search past executions, errors, and decisions via SQL-backed tools — no more grep-only JSONL.
4. **Execution trace completeness:** Every code execution (sandbox.exec, agent.install) has its full result in `execution_traces`, not truncated to 256 chars.
5. **Human readability:** Live digest answers "what happened and why" without cross-referencing other systems.
6. **Agent learning:** An agent encountering an error can query past sessions for similar errors and their resolutions.
7. **Fewer redundancies:** Gateway causal chain deleted. Session timeline replaced. memory.db merged. Session snapshot subsumed.
8. **Artifact robustness + ergonomics:** Agents can use short scoped refs in prompts, while gateway always resolves to canonical digest with integrity checks.
9. **Human interaction is first-class:** An agent can ask a structured question, suspend, resume from checkpoint with the user's answer, and query that interaction later.
10. **Emergency stop is real:** User, gateway self-protection, or the dedicated emergency-manager agent can request a root-session stop, but the gateway is the component that accepts it, aborts live work, writes an auditable stop record, and does not silently auto-resume.
