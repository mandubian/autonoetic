# Session lifecycle: chain, checkpoints, forks, and the digest

What the gateway records about a session while it runs, and what it can do with
that record afterwards: the hash-chained audit trail, the checkpoints that make
every yield point resumable, the fork that turns a past turn into a new
session, the queryable mirror agents learn from, and the narrative surfaces
built on top.

Split out of `ARCHITECTURE.md`, which keeps the system overview. The room UI
that renders this material is [`room.md`](room.md); the tables behind it are in
[`../../reference/store-schema.md`](../../reference/store-schema.md).

## Causal Chain

All actions are logged to an append-only JSONL audit trail:

```
runtime/history/causal_chain.jsonl
agent_dir/history/causal_chain.jsonl
```

### Entry Structure

The JSONL file is a **witness**, not a second store (#1278): since witness
format `v: 2`, entries carry the payload's *fingerprint* — `payload_hash`
(its SHA-256) and `payload_ref` (the key of its content-addressed copy under
`<history>/payloads/<ref>.json`) — never the payload itself. The file stays
small enough to ship offsite or seal on WORM media, and
`causal_chain::verify_chain` re-derives every entry hash and prev-linkage to
prove the history has not been rewritten. `enforced_rules` is bound into the
entry hash, so enforcement attribution (I-6) is tamper-evident too.

```json
{
  "v": 2,
  "timestamp": "2026-03-15T10:30:00Z",
  "log_id": "uuid-v4",
  "actor_id": "coder.default",
  "session_id": "session-123",
  "turn_id": "turn-abc",
  "event_seq": 42,
  "category": "tool",
  "action": "requested",
  "target": "content_write",
  "status": "SUCCESS",
  "reason": null,
  "enforced_rules": ["R+++3"],
  "payload_hash": "sha256-of-payload",
  "payload_ref": "same-sha256-cas-key",
  "entry_hash": "sha256-over-the-fields-above",
  "prev_hash": "sha256-of-previous-entry"
}
```

Pre-existing segments written before #1278 (`v` absent, payload inline) keep
verifying under their original field set — hash computation dispatches on the
entry's format version, it never re-interprets old entries.

The `event_id` is the universal correlation key: execution traces, session reports, and the observability surface all join back to the causal chain via this field.

### Key Events

| Category | Actions | Description |
|----------|---------|-------------|
| `session` | `start`, `end` | Session lifecycle |
| `llm` | `requested`, `completed` | LLM completion calls |
| `tool` | `requested`, `completed`, `failed` | Tool execution |
| `script` | `started`, `completed`, `failed` | Script agent execution |
| `gateway` | `event.ingest.requested`, `.completed` | Ingress events |
| `memory` | `history.persisted`, `session.forked` | Session checkpointing |

### Trace Commands

```bash
autonoetic trace sessions              # List active sessions
autonoetic trace show <session_id>     # View session timeline
autonoetic trace event <log_id>        # View specific entry
autonoetic trace rebuild <session_id>  # Reconstruct unified timeline
autonoetic trace follow <session_id>   # Watch live events
autonoetic trace fork <session_id>     # Fork from checkpoint
autonoetic trace history <session_id>  # View conversation history
```

---

## Session Checkpoints, Continuations, and Forks

Three interrelated mechanisms enable restarting sessions from a given step:

| Mechanism | Purpose | Storage |
|-----------|---------|---------|
| **Checkpoint** | Universal snapshot at every yield point | `runtime/checkpoints/{session_id}/{turn_id}.checkpoint.json` |
| **Turn Continuation** | Suspend/resume at approval boundaries | `runtime/continuations/{task_id}.json` |
| **Session Fork** | Branch a new session from any checkpoint | Copies checkpoint history to a new session |

### Checkpoints

Universal execution snapshots saved at every yield point for crash recovery and session forking.

#### Checkpoint Structure

```json
{
  "session_id": "session-123",
  "turn_id": "turn-042",
  "turn_counter": 42,
  "history": [...],                    // Full conversation history
  "yield_reason": "Hibernation",       // Why execution stopped
  "loop_guard_state": {...},           // Failure tracking state
  "agent_id": "coder.default",
  "workflow_id": "wf-abc",
  "runtime_lock_hash": "sha256:...",
  "constitution_version": "2026.06.05",
  "constitution_digest": "sha256:...",
  "llm_config_snapshot": {...},
  "tool_registry_version": "...",
  "content_store_refs": [...],
  "pending_tool_state": {...},
  "llm_rounds_consumed": 3,
  "tool_invocations_consumed": 12,
  "tokens_consumed": 4500,
  "estimated_cost_usd": 0.04,
  "created_at": "2026-03-15T10:30:00Z"
}
```

#### Yield Reasons

| Reason | Trigger | Auto-Resume? |
|--------|---------|--------------|
| `Hibernation` | EndTurn / StopSequence between turns | Yes |
| `BudgetExhausted` | Session budget depleted | Yes (after budget reset) |
| `ApprovalRequired` | Tool needs approval gate | Via signed checkpoint |
| `UserInputRequired` | `user_ask` pending answer | Yes (when answered) |
| `EmergencyStop` | Operator circuit breaker | **No** (blocks auto-resume) |
| `MaxTurnsReached` | Loop guard limit | Yes |
| `ManualStop` | Operator/user interrupt | Yes |
| `Error` | Recoverable error | Yes |

#### Checkpoint Management

```bash
# List all checkpoints for a session
autonoetic trace checkpoints <session_id>

# View checkpoint details (via the JSON-RPC API or inspecting files)
ls runtime/checkpoints/<session_id>/
```

Checkpoints are pruned automatically (default: keep last N per session).

### Turn Suspension (Approval-Gated Turns)

When a tool call requires operator approval, the turn is **suspended to a signed session checkpoint** rather than failing or retrying with synthetic prompts. On approval, execution resumes seamlessly with real tool results.

#### Suspension Flow

1. Agent requests a privileged tool call (e.g., `agent_revision_promote`, `sandbox_exec` on a new resource)
2. Gateway evaluates policy → approval required
3. Gateway creates an `ApprovalRequest` in SQLite
4. Gateway checkpoints the session with `YieldReason::ApprovalRequired` (HMAC-signed)
5. Turn execution pauses; approval request is emitted

#### Checkpoint Structure

Approval suspension is stored as a `SessionCheckpoint` under `runtime/checkpoints/<session_id>/<turn_id>.checkpoint.json`. The checkpoint is HMAC-SHA256 signed and includes the full conversation history, the pending tool call, remaining tool calls in the batch, and loop-guard state.

#### Resume Flow

1. Operator approves (or rejects) the approval request
2. Gateway applies the decision through `apply_decision`
3. The scheduler wakes the session from checkpoint
4. For `sandbox_exec` approvals: gateway records session approval grants for the detected hosts (enabling auto-approval of subsequent calls to the same hosts within this root session)
5. Gateway injects `approval_ref` into the suspended tool call and resumes the reasoning loop
6. The agent re-issues the tool call with `approval_ref`; the gateway executes it normally and injects the real tool result into conversation history
7. Gateway executes any remaining tool calls from the original batch
8. Checkpoint is deleted after successful resume

### Auto-Resume Behavior

When a session is re-entered (e.g., gateway restart, new event for an existing session), the gateway checks for the latest checkpoint and evaluates whether to auto-resume:

| Yield Reason | Auto-Resume Condition |
|--------------|----------------------|
| `Hibernation` | Always |
| `BudgetExhausted` | Budget available again |
| `MaxTurnsReached` | Always |
| `ManualStop` | Always |
| `Error` | Always |
| `UserInputRequired` | Interaction status is `Answered` |
| `ApprovalRequired` | Via turn continuation (approval resolved) |
| `EmergencyStop` | **Never** — requires manual re-activation |

### Session Forking

Create a new session that starts from the conversation state at any checkpoint, optionally with a branch message for exploring alternative paths.

#### CLI

```bash
# Fork from latest checkpoint
autonoetic trace fork session-123

# Fork from a specific turn
autonoetic trace fork session-123 --at-turn 5

# Fork with a branch message (try a different approach)
autonoetic trace fork session-123 --at-turn 5 --message "try a different approach"

# Fork into a different agent
autonoetic trace fork session-123 --agent researcher.default

# Fork and immediately start chatting
autonoetic trace fork session-123 --at-turn 5 --interactive

# Machine-readable output
autonoetic trace fork session-123 --json
```

#### JSON-RPC API

Method: `session.fork`

```json
{
  "source_session_id": "session-123",
  "branch_message": "optional: try a different approach",
  "new_session_id": "optional: custom-id (auto-generated if omitted)",
  "target_agent_id": "optional: fork into a different agent"
}
```

Response:

```json
{
  "new_session_id": "fork-xxxx",
  "source_session_id": "session-123",
  "fork_turn": 42,
  "history_handle": "sha256:...",
  "message_count": 5
}
```

#### How Forking Works

1. Loads the checkpoint's conversation history from the content store
2. Generates a new session ID (`fork-{uuid}`) or uses the provided one
3. Optionally appends a branch message to the history
4. Stores the history under the new session ID
5. Returns fork metadata (new session ID, source, fork turn, history handle)

Forks can themselves be forked (multi-level branching). Forking fails if no checkpoint exists for the source session.

---

## Queryable Event Store

Causal chain events are mirrored to SQLite for agent learning queries.

### Tables

**`causal_events`** — Queryable mirror of causal chain JSONL:

| Column | Description |
|--------|-------------|
| `event_id` | UUID matching JSONL log_id |
| `agent_id`, `session_id`, `turn_id` | Context |
| `category` | tool_invoke, llm, lifecycle, memory... |
| `action` | requested, completed, failure... |
| `status` | SUCCESS, ERROR, DENIED |
| `enforced_rules` | JSON array of constitutional rule/right IDs this event enforced (default placeholder `R+++3` when none) |
| `target` | Tool name, model name, etc. |
| `payload` | Full JSON (not truncated) |
| `timestamp` | RFC3339 |

#### Principle-aware enforcement events

Enforcement events carry the `P-x.y` / `Ri-x.y` rule/right IDs they enforce in
`enforced_rules`, and (for richer events like `loop_guard.tripped`) the
resolved owning **clause** in the payload. The `enforcement_register`
reverse-maps a `P-x.y` / `Ri-x.y` ID to its owning principle or right,
so breaches correlate by **constitutional clause**, not by ad-hoc rule
strings. See [Contract Health](#contract-health) below.

**`execution_traces`** — Full code execution results:

| Column | Description |
|--------|-------------|
| `trace_id` | UUID |
| `event_id` | Joins to `causal_events.event_id` — the universal correlation key |
| `tool_name` | sandbox_exec, agent_revision_promote... |
| `command` | The executed command |
| `exit_code` | Process exit code |
| `stdout`, `stderr` | Full output (not truncated) |
| `duration_ms` | Execution wall time |
| `success` | Boolean |
| `error_type` | compilation, runtime, permission, validation, resource, conflict, quota_exceeded, not_found, timeout |

### Agent Learning Tools

**`execution_search`** — Query past executions:
```json
{
  "tool_name": "sandbox_exec",
  "success": false,
  "error_type": "compilation",
  "command_pattern": "%client.rs%",
  "limit": 5
}
```

**`knowledge_search`** (with `tags`) — AND-match tagged memories:
```json
{
  "tags": ["type:error_lesson", "domain:http"],
  "limit": 10
}
```

---

## Contract Health

Trust-through-predictability holds only if breaches are detected and corrected —
so "report and correct" is a peer of "constrain." The contract-health view is
the standing tally behind that half of the loop: how often each constitutional
clause (principle/right) has actually been enforced.

It reads the `enforced_rules` carried on `causal_events`, attributes each
`P-x.y` / `Ri-x.y` rule/right ID to its owning clause via the
`enforcement_register` (`clause_of_rule`), and tallies occurrences per clause.
The `R+++3` event-attribution placeholder is skipped (every event carries it by
default); rule IDs not present in the register surface as `unattributed`, so
coverage gaps stay visible rather than silently dropped.

- **Code**: `GatewayStore::contract_health(since)` →
  `enforcement_register::ContractHealth { by_clause, unattributed }`
- **CLI**: `autonoetic trace contract-health [--since <RFC3339>] [--json]`

This is the foundation for principle-aware sentinel correlation; see
`docs/proposals/divergence-sentinel.md`.

---

## Live Digest

Real-time session narrative replacing the flat timeline.md.

### Storage

```
runtime/sessions/{session_id}/digest.md
```

### Structure

```markdown
# Session Digest: {session_id}
Agent: {agent_id} | Started: {timestamp}

---

#### Turn 1 — {timestamp}
**Action:** Called `sandbox_exec` with `python3 tests/run_all.py`
**Result:** 12 tests passed, 1 failed
**Reasoning:** Running full test suite first.

#### Turn 2 — {timestamp}
**Action:** Edited `src/http/client.rs`
**Error:** Compilation failed — missing `Send` bound
**Fix:** Added `+ Send` to trait bound
**Artifact:** Modified `src/http/client.rs` (art_8f2a)
```

### Tools

- **`digest_annotate`** — Agent adds reasoning/decision notes
- **`digest_query`** — Search past session digests

### Session Room

The **canonical timeline** (`live_digest_events`) built from the live digest is
the spine of the **Session Room** — a channel-agnostic, importance-ranked,
multi-actor view of a session that channels (the terminal TUI, and external
bridges) consume as gateway API clients. See
[Session Room — Architecture](room.md) and the
[user guide](../../guide/session-room.md).

---

## Session Read Cache

A per-session, in-memory result cache for **pure read tools** memoizes deterministic reads so an agent that re-reads the same handle across turns does not re-execute the tool or re-inject identical content into the transcript. It lives on `GatewayStore` (`session_read_cache`) keyed by exact `session_id`, and is consulted in `ToolCallProcessor::execute_tool_call` *before* dispatch.

| Tool | Policy | Invalidated by |
|---|---|---|
| `resolve` | Cache forever in-session (content-addressed) | never |
| `agent_inspect` | Cache | `skill_install`, `agent_revision_create`, `agent_revision_create_from_intent`, `agent_revision_promote`, `agent_revision_rollback` |
| `artifact_inspect` | Cache | `artifact_build` |

Properties:

- **Keyed by exact session id**, not root — a cached `resolve` result is never served to a sibling session, preserving per-session content visibility.
- **Wraps only the raw `registry.execute` output**; disclosure registration and secret redaction still run on every hit, so caching is transparent to those invariants.
- **Bounded + size-guarded**: per-session LRU of 128 entries; results over 1 MiB are never stored.
- **Invalidation is coarse but correct**: a mutating tool clears the affected tag class (`AgentExistence` / `ArtifactMetadata`) across *all* session caches, so a child session's promote invalidates the parent's `agent_inspect` cache. `resolve` is never invalidated.
- **Audited**: a cache hit emits a `tool_call.cache_hit` causal event (and the normal execution trace still records), so the causal chain shows every logical tool call.

Grounding: extends the determinism-skip principle of P-2.6 / P-2.7 (approved-execution caching) to pure reads, where the safety argument is stronger — there is no side effect to skip.

---
