# Workflow & Approval Hardening Plan

## Context

The workflow orchestration infrastructure (P1–P8 in `plan_workflow_orchestration.md`) is implemented and working: async spawn, queued tasks, join conditions, checkpoints, workflow events, causal chain mirroring, CLI graph. What remains fragile is the **signal delivery, approval lifecycle, and notification pipeline** that connects approval decisions back to the workflow engine and its consumers.

This plan addresses the fragility discovered during the code review and aligns with the findings already documented in `plan_signal.md`.

## Guiding Principles

1. **CLI is a consumer, not an orchestrator.** CLI chat, WhatsApp adapters, webhooks — all are thin consumers. No critical workflow/approval logic should live in any consumer.
2. **Approval channel is separate from chat.** The approver could be a human operator, a CLI command, or a dedicated approval agent. Approval resolution must not depend on a chat session being open.
3. **Gateway owns delivery.** The gateway daemon is the single durable owner of notification delivery and action execution after approval.
4. **SQLite for queue-like state, files for per-entity state.** Queue-like access patterns (notifications, approvals, workflow events) benefit from SQLite's atomic transactions and concurrent-access handling. Per-entity state (workflow runs, task runs, checkpoints) can remain as files since their access pattern is isolated per workflow.

## Architecture: SQLite-Backed Gateway Store

### Why SQLite now

- `rusqlite` with `bundled` is already a dependency in `autonoetic-gateway/Cargo.toml`
- `Tier2Memory` in `memory.rs` establishes the usage pattern
- No backward compatibility needed — autonoetic is new, we can break anything
- The signal file system has queue-like semantics (concurrent producers/consumers, dedup, TTL) that files handle poorly

### What moves to SQLite

A single `gateway.db` file at `runtime/gateway.db` replaces:

| Current file-based system | SQLite table |
|--------------------------|-------------|
| `runtime/scheduler/approvals/pending/*.json` | `approvals` table |
| `runtime/scheduler/approvals/approved/*.json` | `approvals` table (status column) |
| `runtime/scheduler/approvals/rejected/*.json` | `approvals` table (status column) |
| `runtime/signal/{session_id}/*.json` | `notifications` table |
| `runtime/signal/{session_id}/*.delivered` | `notifications` table (status column) |
| `runs/{wf_id}/events.jsonl` | `workflow_events` table |

### What stays as files

- `runs/{wf_id}/workflow.json` — per-workflow state, isolated access
- `runs/{wf_id}/tasks/*.json` — per-task state, isolated access
- `runs/{wf_id}/checkpoints/` — per-workflow/task checkpoints
- `runs/{wf_id}/queue/*.json` — queued task records (could migrate later)
- `runtime/scheduler/approvals/pending/*_payload.json` — large install payloads (blob, not queried)

---

## Phase W1: Gateway Store (SQLite backbone) [DONE]

### [x] W1.1 — Create `GatewayStore` struct

**File**: `autonoetic-gateway/src/scheduler/gateway_store.rs` [NEW]

```rust
pub struct GatewayStore {
    conn: rusqlite::Connection,
}

impl GatewayStore {
    pub fn open(gateway_dir: &Path) -> anyhow::Result<Self> {
        let db_path = gateway_dir.join("gateway.db");
        let conn = Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    fn migrate(conn: &Connection) -> anyhow::Result<()> {
        // Creates all tables if not exists
    }
}
```

Tables:

```sql
CREATE TABLE IF NOT EXISTS approvals (
    request_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    root_session_id TEXT,
    workflow_id TEXT,
    task_id TEXT,
    action_type TEXT NOT NULL,        -- 'agent_install', 'sandbox_exec', 'write_file', 'generic'
    action_payload TEXT NOT NULL,     -- JSON
    reason TEXT,
    evidence_ref TEXT,
    status TEXT NOT NULL DEFAULT 'pending',  -- 'pending', 'approved', 'rejected'
    created_at TEXT NOT NULL,
    decided_at TEXT,
    decided_by TEXT
);

CREATE TABLE IF NOT EXISTS notifications (
    notification_id TEXT PRIMARY KEY,
    notification_type TEXT NOT NULL,   -- 'approval_resolved', 'workflow_join_satisfied'
    request_id TEXT,                   -- FK to approvals (nullable)
    target_session_id TEXT NOT NULL,
    target_agent_id TEXT,
    workflow_id TEXT,
    task_id TEXT,
    payload TEXT NOT NULL,             -- full structured JSON (same shape everywhere)
    status TEXT NOT NULL DEFAULT 'pending',  -- 'pending', 'action_executed', 'delivered', 'consumed'
    created_at TEXT NOT NULL,
    action_completed_at TEXT,
    delivered_at TEXT,
    consumed_at TEXT,
    attempt_count INTEGER NOT NULL DEFAULT 0,
    last_attempt_at TEXT,
    error_message TEXT
);

CREATE TABLE IF NOT EXISTS workflow_events (
    event_id TEXT PRIMARY KEY,
    workflow_id TEXT NOT NULL,
    event_type TEXT NOT NULL,          -- 'task.spawned', 'task.completed', etc.
    task_id TEXT,
    agent_id TEXT,
    payload TEXT,                      -- JSON
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_approvals_status ON approvals(status);
CREATE INDEX IF NOT EXISTS idx_approvals_session ON approvals(session_id);
CREATE INDEX IF NOT EXISTS idx_approvals_root_session ON approvals(root_session_id);
CREATE INDEX IF NOT EXISTS idx_approvals_workflow ON approvals(workflow_id);
CREATE INDEX IF NOT EXISTS idx_notifications_status ON notifications(status);
CREATE INDEX IF NOT EXISTS idx_notifications_target ON notifications(target_session_id);
CREATE INDEX IF NOT EXISTS idx_workflow_events_workflow ON workflow_events(workflow_id);
CREATE INDEX IF NOT EXISTS idx_workflow_events_created ON workflow_events(created_at);
```

### [x] W1.2 — Approval CRUD on GatewayStore

**File**: `autonoetic-gateway/src/scheduler/gateway_store.rs` [NEW, continued]

Methods:

```rust
impl GatewayStore {
    // Approvals
    pub fn create_approval(&self, request: &ApprovalRequest) -> Result<()>;
    pub fn get_approval(&self, request_id: &str) -> Result<Option<ApprovalRequest>>;
    pub fn list_pending_approvals(&self) -> Result<Vec<ApprovalRequest>>;
    pub fn list_pending_approvals_for_root(&self, root_session_id: &str) -> Result<Vec<ApprovalRequest>>;
    pub fn list_pending_approvals_for_session(&self, session_id: &str) -> Result<Vec<ApprovalRequest>>;
    pub fn record_decision(&self, request_id: &str, status: &str, decided_by: &str) -> Result<()>;

    // Notifications
    pub fn create_notification(&self, notification: &NotificationRecord) -> Result<()>;
    pub fn list_pending_notifications(&self) -> Result<Vec<NotificationRecord>>;
    pub fn list_notifications_for_session(&self, session_id: &str, status: &str) -> Result<Vec<NotificationRecord>>;
    pub fn update_notification_status(&self, id: &str, status: &str) -> Result<()>;
    pub fn mark_action_executed(&self, id: &str) -> Result<()>;
    pub fn mark_delivered(&self, id: &str) -> Result<()>;
    pub fn mark_consumed(&self, id: &str) -> Result<()>;
    pub fn increment_attempt(&self, id: &str, error: Option<&str>) -> Result<()>;

    // Workflow events
    pub fn append_workflow_event(&self, event: &WorkflowEventRecord) -> Result<()>;
    pub fn list_workflow_events(&self, workflow_id: &str) -> Result<Vec<WorkflowEventRecord>>;
    pub fn list_workflow_events_since(&self, workflow_id: &str, since: &str) -> Result<Vec<WorkflowEventRecord>>;

    // Cleanup
    pub fn cleanup_stale_notifications(&self, max_age_hours: u64) -> Result<u64>;
}
```

### [x] W1.3 — Wire `GatewayStore` into `GatewayConfig` / startup

**File**: `autonoetic-types/src/config.rs` [MODIFY]
**File**: `autonoetic-gateway/src/server/mod.rs` [MODIFY]

- Open `GatewayStore` at gateway startup
- Pass it (as `Arc<GatewayStore>`) to the scheduler and approval functions
- All approval/notification operations go through `GatewayStore` methods

---

## Phase W2: Rework Approval Lifecycle

### W2.1 — `create_approval` writes to SQLite (not files)

**File**: `autonoetic-gateway/src/scheduler/approval.rs` [MODIFY]

Replace all file-based approval creation (`write pending/*.json`) with `store.create_approval()`.

The `ApprovalRequest` struct gains explicit `workflow_id` and `task_id` (resolved at creation time via `resolve_workflow_id_for_root_session` + `resolve_task_id_for_session`).

### W2.2 — Split `approve_request` into decision + notification

**File**: `autonoetic-gateway/src/scheduler/approval.rs` [MODIFY]

Current `approve_request()` does 4 things synchronously. Split:

1. `record_decision(request_id, "approved", decided_by)` — updates SQLite row
2. `create_notification(...)` — inserts a `Pending` notification row
3. **Return immediately** — gateway scheduler handles the rest

The scheduler tick (W3) processes the notification: executes the action, unblocks the workflow task, marks delivered.

### W2.3 — Remove file-based approval system

**File**: `autonoetic-gateway/src/scheduler/approval.rs` [MODIFY]

Delete:
- `load_all_pending_approvals()` (file-based scan) → replaced by `store.list_pending_approvals()`
- `load_all_approved_decisions()` (file-based scan) → replaced by `store.get_approval()` with status check
- All `pending/*.json` / `approved/*.json` / `rejected/*.json` directory management
- `resume_session_after_approval()` — replaced by notification processing in scheduler

Keep:
- `execute_approved_install()` — moved to notification processor
- `execute_approved_sandbox_exec()` — moved to notification processor
- `unblock_task_on_approval()` — moved to notification processor
- Install payload files (`*_payload.json`) — these are large blobs, stay as files

### W2.4 — Bind approvals to workflow explicitly

When creating an `ApprovalRequest` (from `agent.install`, `sandbox_exec`):
- Resolve `workflow_id` from session → root session → workflow index
- Resolve `task_id` via `resolve_task_id_for_session()`
- Store both on the approval row

In `unblock_task_on_approval`: use `(workflow_id, task_id)` directly instead of scanning by session_id.

---

## Phase W3: Gateway-Owned Notification Processing

### W3.1 — Add `process_pending_notifications()` to scheduler tick

**File**: `autonoetic-gateway/src/scheduler.rs` [MODIFY]

Add to `run_scheduler_tick()`:

```rust
fn process_pending_notifications(store: &GatewayStore, config: &GatewayConfig) {
    let pending = store.list_pending_notifications().unwrap_or_default();
    for notification in pending {
        match notification.notification_type {
            NotificationType::ApprovalResolved => {
                // 1. Execute action if needed (install/sandbox/write_file)
                let action_result = execute_approved_action(&notification, config);
                match action_result {
                    Ok(()) => store.mark_action_executed(&notification.notification_id),
                    Err(e) => {
                        store.increment_attempt(&notification.notification_id, Some(&e.to_string()));
                        continue; // retry next tick
                    }
                }
                // 2. Unblock workflow task
                if let (Some(wf_id), Some(task_id)) = (&notification.workflow_id, &notification.task_id) {
                    unblock_task_on_approval(config, wf_id, task_id, &notification);
                }
                // 3. Mark delivered (consumer-visible)
                store.mark_delivered(&notification.notification_id);
            }
            NotificationType::WorkflowJoinSatisfied => {
                // No action to execute, just mark delivered
                store.mark_delivered(&notification.notification_id);
            }
        }
    }
}
```

### W3.2 — Notification cleanup in scheduler tick

Add to scheduler tick:

```rust
// Clean up stale notifications (consumed > 1h ago, or failed with > 10 attempts)
store.cleanup_stale_notifications(24);
```

---

## Phase W4: Simplify Signal System → Consumer Notification API

### W4.1 — Replace signal.rs with consumer notification queries

**File**: `autonoetic-gateway/src/scheduler/signal.rs` [MAJOR REWRITE]

Delete:
- `start_signal_poller_if_needed()` — background TCP delivery thread
- `poll_pending_signals()` — directory scanning / TCP delivery
- `deliver_signal()` — raw TCP JSON-RPC writing
- `write_signal()` / `check_pending_signals()` / `consume_signal()` — file-based signal lifecycle
- `should_defer_to_client_consumer()` — grace window hack
- `collect_session_directories()` — recursive directory scan
- All TCP connection code

Replace with thin wrappers over `GatewayStore`:

```rust
/// Consumer API: list notifications ready for display.
pub fn consumer_notifications(store: &GatewayStore, session_id: &str) -> Vec<NotificationRecord> {
    store.list_notifications_for_session(session_id, "delivered").unwrap_or_default()
}

/// Consumer API: acknowledge a notification after display.
pub fn ack_notification(store: &GatewayStore, notification_id: &str) {
    store.mark_consumed(notification_id);
}
```

**Net effect**: `signal.rs` goes from ~727 lines to ~50 lines.

### W4.2 — Simplify CLI chat signal handling

**File**: `autonoetic/src/cli/chat.rs` [MODIFY]

`check_signals()` becomes:

```rust
async fn check_signals(app: &mut App, store: &GatewayStore, session_id: &str) -> bool {
    let notifications = consumer_notifications(store, session_id);
    let mut processed = false;
    for n in notifications {
        match n.notification_type {
            NotificationType::ApprovalResolved => {
                let icon = if n.payload["status"] == "approved" { "✅" } else { "❌" };
                app.add_message(MessageRole::Signal, format!("{} {}", icon, n.payload["message"]));
                app.resolve_awaiting_approval(&n.payload["request_id"]);
            }
            NotificationType::WorkflowJoinSatisfied => {
                app.add_message(MessageRole::Signal, format!("✅ Workflow join satisfied"));
            }
        }
        ack_notification(store, &n.notification_id);
        processed = true;
    }
    processed
}
```

Delete from `chat.rs`:
- `signal_resume_inflight` / `signal_resume_by_internal_id` tracking
- Building and sending resume `event.ingest` payloads
- `SignalResumeRef` struct
- All the 200+ lines of signal-to-pending-map coordination

**The CLI never sends `event.ingest` for approval resume.** The gateway already executed the action. The CLI just displays and acks.

### W4.3 — Update workflow event emission to use SQLite

**File**: `autonoetic-gateway/src/scheduler/workflow_store.rs` [MODIFY]

Replace `append_jsonl_record()` calls for workflow events with `store.append_workflow_event()`.

The `WorkflowEventStream` (polling JSONL by line count) gets replaced by:

```rust
pub fn workflow_events_since(store: &GatewayStore, workflow_id: &str, since: &str) -> Vec<WorkflowEventRecord> {
    store.list_workflow_events_since(workflow_id, since).unwrap_or_default()
}
```

The existing `events.jsonl` file can still be written for audit/backup, but the live query path goes through SQLite.

---

## Phase W5: Clean Up and Doc Update

### W5.1 — Remove dead file-based signal/approval infrastructure

Files/functions to delete after W1–W4:
- `start_signal_poller_if_needed()` and all callers
- `PendingSignal`, `Signal` enum (replaced by `NotificationRecord`)
- File-based approval directory scanning functions
- `mark_signal_delivered` / `is_signal_delivered` marker file logic
- `consume_all_signals()`

### W5.2 — Update docs

**Files**: `docs/approval-notification-delivery.md`, `docs/workflow-orchestration.md`, `docs/agent-install-approval-retry.md`

Update to describe:
- Single `gateway.db` as the notification/approval store
- Consumer notification protocol (poll `consumer_notifications`, display, ack)
- For adapter authors: how to query the store for a WhatsApp/webhook adapter

### W5.3 — Update `GatewayStore` usage in CLI approval commands

**File**: `autonoetic/src/cli/gateway.rs` [MODIFY]

`gateway approvals list|approve|reject` commands read/write via `GatewayStore` instead of file manipulation.

---

## Phase W6: CLI Chat Improvements (post-W1–W5)

Once the gateway owns all notification delivery and the CLI is a pure display consumer, these improvements become straightforward.

### W6.1 — Replay past workflow events on reconnect

**File**: `autonoetic/src/cli/chat.rs` [MODIFY]

Currently, the bootstrap tick silently swallows all existing workflow events (`workflow_events_bootstrapped` flag suppresses display). After the SQLite migration, on reconnect:

1. Query `store.list_workflow_events(workflow_id)` for the session's workflow
2. Display the last N events (e.g., 20) as a compact recap block with a "--- session reconnected ---" separator
3. Then start incremental display for new events

This gives the operator context on what happened while they were away.

```rust
// On reconnect, show last N workflow events as recap
if !app.workflow_events_bootstrapped {
    let events = store.list_workflow_events(&workflow_id)?;
    let recap_count = events.len().min(20);
    if recap_count > 0 {
        app.add_message(MessageRole::System, "── workflow recap ──".to_string());
        for event in events.iter().rev().take(recap_count).rev() {
            if let Some(card) = format_workflow_event_card(event) {
                app.add_message(MessageRole::Signal, card);
            }
            app.seen_workflow_event_ids.insert(event.event_id.clone());
        }
        app.add_message(MessageRole::System, "── live updates ──".to_string());
    }
    app.workflow_events_bootstrapped = true;
}
```

### W6.2 — Fix `select!` priority starvation for signal checks

**File**: `autonoetic/src/cli/chat.rs` [MODIFY]

The current `tokio::select!` gives highest priority to `gateway_lines.next_line()`, which can starve signal/notification checks during long agent responses. Use `tokio::select!` with `biased;` removed (fair polling) or interleave a notification check between gateway line reads:

```rust
tokio::select! {
    // Use biased only for the sleep branch to keep UI responsive;
    // gateway_lines and signal_interval have equal priority
    result = gateway_lines.next_line() => { /* ... */ }
    _ = signal_interval.tick() => {
        if check_signals(app, store, session_id).await {
            needs_redraw = true;
        }
    }
    msg = rx.recv() => { /* ... */ }
    _ = tokio::time::sleep(Duration::from_millis(16)) => { /* TUI input */ }
}
```

Additionally, reduce `signal_interval` from 3 seconds to 1 second — SQLite queries are cheap (~1ms) compared to the old file system scans.

### W6.3 — Persistent workflow status bar

**File**: `autonoetic/src/cli/chat.rs` [MODIFY]

The `workflow_status_line` is currently updated inside `check_signals()` every 3 seconds. After the SQLite migration:

1. Query task counts directly from `GatewayStore` (or from task files, which remains efficient)
2. Show the status bar as a persistent ratatui row at the top of the chat panel (currently it's only visible in some layouts)
3. Add approval-count indicator: `⏸ 2 approvals pending` when `awaiting_approvals` is non-empty
4. Show elapsed time since last activity

### W6.4 — Remove in-memory dedup state, use SQLite notification status

**File**: `autonoetic/src/cli/chat.rs` [MODIFY]

Delete after W4.2 lands:
- `seen_workflow_event_ids: HashSet<String>` — replaced by `store.list_workflow_events_since(workflow_id, last_seen_timestamp)` 
- `announced_pending_approvals: HashSet<String>` — replaced by querying `store.list_pending_approvals_for_root()`
- `awaiting_approvals: HashSet<String>` — can be rebuilt from `store.list_pending_approvals_for_root()` on each tick
- `workflow_events_bootstrapped: bool` — replaced by timestamp-based incremental query

This means the CLI can crash and reconnect without any state loss. All dedup is in the database.

### W6.5 — Auto-reconnect to gateway on disconnect

**File**: `autonoetic/src/cli/chat.rs` [MODIFY]

Currently, `Ok(None)` from `gateway_lines.next_line()` breaks the loop with "Gateway disconnected". Instead:

1. Show "⚠ Gateway disconnected, reconnecting in 3s..."
2. Attempt TCP reconnect with exponential backoff (3s, 6s, 12s, max 30s)
3. On successful reconnect, re-send the session envelope and restore the reader/writer
4. Continue the event loop — the user doesn't lose their chat history
5. Signal/notification checks continue working during disconnection (they read SQLite directly, no TCP needed)

This is critical for long-running workflow sessions where the gateway might restart.

---

## Proposed Changes Summary

### [NEW] Files

| File | Purpose |
|------|---------|
| `autonoetic-gateway/src/scheduler/gateway_store.rs` | SQLite-backed store for approvals, notifications, workflow events |
| `autonoetic-types/src/notification.rs` | `NotificationRecord`, `NotificationType`, `NotificationStatus` types |

### [MODIFY] Files

| File | Changes |
|------|---------|
| `autonoetic-gateway/src/scheduler/approval.rs` | Remove file-based approval system, use `GatewayStore`; split `approve_request` into decision + notification |
| `autonoetic-gateway/src/scheduler/signal.rs` | Major rewrite: remove TCP delivery, polling, file management; replace with thin consumer API over `GatewayStore` |
| `autonoetic-gateway/src/scheduler.rs` | Add `process_pending_notifications()` to scheduler tick |
| `autonoetic-gateway/src/scheduler/workflow_store.rs` | Workflow event emission through `GatewayStore`; simplify `WorkflowEventStream` |
| `autonoetic/src/cli/chat.rs` | Simplify signal handling (W4.2); workflow recap on reconnect (W6.1); fix `select!` starvation (W6.2); persistent status bar (W6.3); remove in-memory dedup (W6.4); auto-reconnect (W6.5) |
| `autonoetic/src/cli/gateway.rs` | Approval commands use `GatewayStore` |
| `autonoetic-types/src/config.rs` | Optional: add `GatewayStore` reference or path config |
| `autonoetic-gateway/src/server/mod.rs` | Initialize `GatewayStore` at startup |

### [DELETE] (contents replaced, files may remain as thin wrappers)

| Current content | Reason |
|---------------|--------|
| Signal file poller + TCP delivery (~400 lines in `signal.rs`) | Replaced by scheduler notification processing |
| File-based approval scanning (~200 lines in `approval.rs`) | Replaced by SQLite queries |
| CLI signal resume coordination (~200 lines in `chat.rs`) | Consumers display only |
| `should_defer_to_client_consumer` grace window | Not needed with single-owner model |

**Estimated net: ~800 lines deleted, ~400 lines added.**

---

## Implementation Order

| Step | Phase | Effort | Depends On |
|------|-------|--------|-----------|
| 1 | W1.1–W1.2 — `GatewayStore` struct + SQLite tables | Medium | — |
| 2 | W1.3 — Wire into startup + config | Small | W1.1 |
| 3 | W2.1 — Approval creation via SQLite | Medium | W1.1 |
| 4 | W2.2 — Split approve_request | Medium | W2.1 |
| 5 | W2.3 — Remove file-based approval system | Medium | W2.1, W2.2 |
| 6 | W2.4 — Explicit workflow binding | Small | W2.1 |
| 7 | W3.1 — Scheduler processes notifications | Medium | W2.2, W1.1 |
| 8 | W4.1 — Rewrite signal.rs | Medium | W3.1 |
| 9 | W4.2 — Simplify CLI chat | Medium | W4.1 |
| 10 | W4.3 — Workflow events via SQLite | Small | W1.1 |
| 11 | W5.1–W5.3 — Cleanup + docs | Small | All above |
| 12 | W6.1 — Replay past workflow events on reconnect | Small | W4.2,W4.3 |
| 13 | W6.2 — Fix `select!` priority starvation | Small | W4.2 |
| 14 | W6.3 — Persistent workflow status bar | Small | W4.2 |
| 15 | W6.4 — Remove in-memory dedup state | Small | W4.2 |
| 16 | W6.5 — Auto-reconnect to gateway | Medium | W4.2 |

Steps 1–2 can be built and tested independently. Steps 3–7 form the core refactor. Steps 8–9 are the consumer simplification. Steps 10–11 are polish. Steps 12–16 are CLI UX improvements that can land incrementally after the core is done.

---

## Verification Plan

### Existing tests to validate no regressions

```bash
# Unit tests in approval module
cargo test -p autonoetic-gateway approval -- --nocapture

# Unit tests in signal module
cargo test -p autonoetic-gateway signal -- --nocapture

# Unit tests in workflow_store module
cargo test -p autonoetic-gateway workflow_store -- --nocapture

# E2E approval test
cargo test -p autonoetic-gateway --test agent_install_approval_e2e -- --nocapture

# Background scheduler integration
cargo test -p autonoetic-gateway --test background_scheduler_integration -- --nocapture
```

> [!IMPORTANT]
> Several existing tests will need updating because they directly manipulate approval files in `pending/` / `approved/` directories. These tests should be migrated to use `GatewayStore` methods instead.

### New tests to add

#### 1. `gateway_store_approval_lifecycle` (unit, in `gateway_store.rs`)

Create approval → list pending → record decision → verify status updated → list pending is empty.

```bash
cargo test -p autonoetic-gateway gateway_store_approval_lifecycle -- --nocapture
```

#### 2. `gateway_store_notification_lifecycle` (unit, in `gateway_store.rs`)

Create notification → list pending → mark action_executed → mark delivered → mark consumed → verify cleanup.

```bash
cargo test -p autonoetic-gateway gateway_store_notification_lifecycle -- --nocapture
```

#### 3. `approval_creates_notification` (unit, in `approval.rs`)

Call `record_approval_decision()` → verify notification record created with status `pending` → verify no action executed yet.

```bash
cargo test -p autonoetic-gateway approval_creates_notification -- --nocapture
```

#### 4. `scheduler_processes_pending_notification` (integration)

Create pending notification with `AgentInstall` action → run scheduler tick → verify install executed and notification status is `delivered`.

```bash
cargo test -p autonoetic-gateway scheduler_processes_pending_notification -- --nocapture
```

#### 5. `failed_action_retried_on_next_tick` (unit, in `gateway_store.rs`)

Create notification → simulate failed action (increment attempt) → verify attempt_count increased → process again → verify success.

```bash
cargo test -p autonoetic-gateway failed_action_retried -- --nocapture
```

#### 6. `workflow_events_via_sqlite` (unit, in `gateway_store.rs`)

Append events → query by workflow_id → query with since filter → verify ordering.

```bash
cargo test -p autonoetic-gateway workflow_events_via_sqlite -- --nocapture
```

### Manual Verification

After implementing W1–W4:

1. Start gateway daemon
2. Open `autonoetic chat`
3. Trigger an agent install requiring approval
4. In a separate terminal, run `autonoetic gateway approvals approve <id>`
5. **Verify**: Chat displays the approval resolution notification (no duplicate)
6. **Verify**: Agent is installed (`agents/` directory)
7. **Verify**: `autonoetic trace graph` shows correct task status transitions
    8. **Verify**: `runtime/gateway.db` exists and contains the approval/notification records (can check with `sqlite3 runtime/gateway.db "SELECT * FROM approvals;"`)

---

## Implementation Status (as of 2026-03-22)

### ✅ Completed Phases

| Phase | Summary | Status |
|-------|---------|--------|
| **W1** | `GatewayStore` SQLite backbone — schema, CRUD for approvals, notifications, workflow events | ✅ Done |
| **W2** | Approval lifecycle reworked — SQLite-backed create/decide/list, file-based code removed | ✅ Done |
| **W3** | Gateway-owned notification processing — scheduler tick delivers and cleans up notifications | ✅ Done |
| **W4** | `signal.rs` rewritten as thin SQLite wrapper; workflow events migrated to `workflow_events` table | ✅ Done |
| **W5** | Dead file-based code removed; architectural docs updated; CLI approval commands updated | ✅ Done |
| **W6** | CLI chat improvements — workflow recap on reconnect, anti-starvation select! fix, persistent status bar, auto-reconnect | ✅ Done |

### ✅ Post-Review Fixes Applied (2026-03-22)

The following bugs were found and fixed during code review:

| File | Issue | Fix |
|------|-------|-----|
| `gateway_store.rs` | `.unwrap()` on `serde_json::from_str` in `list_workflow_events` and `list_workflow_events_since` — panic on corrupt DB row | Replaced with `.map_err(→ FromSqlConversionFailure)` |
| `chat.rs` | Signal poll interval was 3 s (should be 1 s per W6.2) | Changed to `Duration::from_secs(1)` |
| `chat.rs` | `let _needs_redraw_unused = true` — key presses did not trigger a redraw | Fixed to `needs_redraw = true` |
| `chat.rs` | Dead `_gateway_dir` variable left over from file-based signal era | Removed |
| `chat.rs` | "Connected to gateway" message shown before the reconnect loop actually connected | Changed to "Connecting to…" |

### ⚠️ Remaining Test Compilation Issues

The W5 refactoring (SQLite migration, `gateway_store` arg additions, `ScheduledAction::AgentInstall { payload }` field) introduced arity mismatches in the integration test suite that `cargo check` does not catch (tests are separate compilation units). Progress so far:

| Category | Count | Status |
|----------|-------|--------|
| `.execute()` takes 10 args, test passes 9 — missing `gateway_store: None` | ~33 | 🔄 Partially patched (sed/perl/tool edits in progress) |
| `AgentExecutor::new` takes 6 args, test passes 5 — missing `gateway_store: None` | ~3 | 🔄 Partially patched |
| `ScheduledAction::AgentInstall` missing `payload: None` field | 3 | ✅ Fixed |
| `CausalLogger` not imported in `promotion_lookup.rs` tests | 1 | ✅ Fixed |
| `check_pending_signals` removed — 2 tests using it | 2 | ✅ Rewritten to use `GatewayStore::list_pending_notifications` |
| `execution_service` not in scope in `generated_skill_integration.rs` | 2 | 🔄 Pending |
| `JsonRpcRouter::new` takes 2 args, test passes 1 | 1 | 🔄 Pending |
| `approval.rs` test module unclosed delimiter | 1 | ✅ Fixed |

**Files still needing patches** (remaining `.execute()` 9→10 arg calls):
- `autonoetic-gateway/tests/agent_install_approval_e2e.rs` (lines 567, 596)
- `autonoetic-gateway/tests/approved_exec_cache_integration.rs` (lines 369, 378 — sed partially applied wrong indentation)
- `autonoetic-gateway/tests/disclosure_integration.rs` (lines 168, 200)
- `autonoetic-gateway/tests/session_trace_integration.rs` (lines 133, 252)
- `autonoetic-gateway/tests/promotion_record_reject.rs` (line 231)
- `autonoetic-gateway/src/runtime/lifecycle.rs` (line 970 — `AgentExecutor::new`)
- `autonoetic-gateway/src/server/jsonrpc.rs` (test: `JsonRpcRouter::new` needs store arg)
- `autonoetic-gateway/tests/generated_skill_integration.rs` (`execution_service` variable missing — this one needs to examine how that variable was set up before and wire it correctly)

> [!NOTE]
> The two long-running background `sed` terminal commands (2h+ each) may be holding file locks and causing subsequent shell commands to stall. These should be killed before resuming test patching.
