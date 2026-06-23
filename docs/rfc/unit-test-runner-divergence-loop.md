# RFC: Unit Test Runner Divergence Loop & Stuck-Session Recovery

**Status:** Draft — 2026-06-23
**Origin:** Observed in production: `unit_test_runner` sessions loop without
concluding when test suites fail. The divergence sentinel kills the session
before a verdict is recorded, then clarification children inherit the same
tight loop guard and also diverge. The orphan reaper cancels children without
notifying waiting parents, leaving the planner stuck in `workflow.wait`.

**Related:** `agents/specialists/unit_test_runner.default/SKILL.md`,
`autonoetic-gateway/src/runtime/lifecycle.rs` (LoopGuard + trajectory monitor),
`autonoetic-gateway/src/runtime/trajectory_health.rs`,
`autonoetic-gateway/src/scheduler.rs` (orphan reaper),
`autonoetic-gateway/src/runtime/tools/workflow.rs` (`workflow.wait`).

---

## 1. Problem

When `unit_test_runner` runs a test suite that **legitimately fails**, the
session often loops without concluding:

1. `artifact_exec` runs the suite → tests return non-zero exit code.
2. The LoopGuard treats this as both **no progress** and a **tool failure**
   (non-zero exit code).
3. After 2 such calls (`max_tool_failures: 2`, `max_loops_without_progress: 2`)
   the trajectory monitor classifies health as `Critical`.
4. The sentinel kills the session before the runner can call `promotion_record`
   with its verdict.
5. If a clarification child is spawned, it **inherits the same manifest**
   (same tight loop guard) and can also diverge, creating a chain.
6. The orphan reaper cancels these children but **does not notify** waiting
   parents, so the planner blocks in `workflow.wait` for up to 30 seconds per
   poll cycle.

The root cause is a **category error**: the LoopGuard conflates *tool-execution
failure* (sandbox crash, permission denied) with *domain-result failure* (tests
returned non-zero). For a test runner, a failing test suite is a **correct,
successful tool execution** that produces a verdict the runner should act on.

---

## 2. Detailed analysis

### 2.1 The LoopGuard conflates test failures with tool failures

In `runtime/lifecycle.rs:3167-3189`, each tool result is mapped to a
`ToolObservation`:

```rust
let failed = parsed.as_ref().map_or(false, |v| {
    if v.get("ok").and_then(|o| o.as_bool()) == Some(false) {
        return true;
    }
    if let Some(code) = v.get("exit_code").and_then(|c| c.as_i64()) {
        return code != 0;
    }
    false
});
```

A non-zero `exit_code` sets `failed = true`, which:
- Prevents `register_progress()` from being called → increments
  `current_loops`.
- Increments `tool_failure_counts["artifact_exec"]` → counts toward
  `max_tool_failures`.

For `unit_test_runner`, a non-zero exit code from `artifact_exec` means the
**tests** failed, not the **tool**. The tool executed correctly and returned
structured output the runner needs. But the LoopGuard treats it identically to
a sandbox crash.

### 2.2 The loop guard budget is too tight for the runner's workflow

```yaml
# unit_test_runner.default/SKILL.md
loop_guard:
  max_loops_without_progress: 2
  max_tool_failures: 2
  max_session_turns: 4
```

The runner's intended workflow is: `artifact_inspect` → discover tests →
`artifact_exec` → parse results → `promotion_record` verdict. That can easily
take 4+ turns. With `max_session_turns: 4` and divergence tripping at 2 loops,
the runner is killed before it finishes.

### 2.3 Clarification children inherit the parent's loop guard

`spawn_clarification_for_approval` (`execution.rs:3193-3326`) loads the parent
agent's manifest and runs an `AgentExecutor` with
`.with_initial_session_state(SessionState::Clarification)`. Clarification mode
clamps tools to read-only inspection tools, but the **LoopGuard config is
unchanged** — `max_loops_without_progress: 2` still applies. If the LLM loops
on read-only tools (e.g. repeated `observability_search`), the clarification
child diverges and fails, prompting another clarification, creating a chain.

### 2.4 The orphan reaper does not notify waiting parents

`reap_orphaned_sessions` (`scheduler.rs:698-879`) cancels a child's `TaskRun`
by writing `TaskRunStatus::Cancelled` directly to the JSON file
(`scheduler.rs:818-866`). It **bypasses the normal notification path**
(`task_notify.notify_session`), so a parent blocked in `workflow.wait`
(`runtime/tools/workflow.rs:480-667`) only discovers the cancellation via its
5-second fallback poll or its own `timeout_secs` deadline.

### 2.5 Transcript/TaskRun status can drift

`AgentExecutor::close_session` finalizes the SQLite `session_transcripts` row
to `failed` **inside** `spawn_agent_once`. Only **after** `spawn_agent_once`
returns does the scheduler call `update_task_run_status` to move the JSON
`TaskRun` out of `Running`. If the process crashes between these two steps, the
transcript is `failed` but the `TaskRun` remains `Running`. `workflow.wait`
polls the `TaskRun`, so it blocks until `check_stuck_running_tasks` (default
600 s) or its own timeout.

---

## 3. Proposed changes

### Change 1: Decouple tool-execution success from domain-result failure

**Priority:** P0 — this is the root-cause fix.
**Files:** `autonoetic-gateway/src/runtime/lifecycle.rs:3167-3189`,
`autonoetic-gateway/src/runtime/guard.rs`

A tool that **executed and returned structured output** should count as
progress, regardless of the test exit code. The distinction:

| Condition | Current classification | Proposed classification |
|---|---|---|
| `ok: false` with `error_type` | tool failure + no progress | **unchanged** — real failure |
| `ok: true`, `exit_code != 0` | tool failure + no progress | **success + progress** — tool ran fine, domain result is negative |
| `ok: true`, `exit_code == 0` | success + progress | **unchanged** |

Implementation: in the `ToolObservation` construction, do not set `failed = true`
for `exit_code != 0` when `ok == true`. The tool executed successfully; the test
suite result is a domain outcome the runner should process, not a mechanical
failure.

This single change lets the runner run tests, see failures, and record its
verdict normally instead of being killed for diverging.

### Change 2: Relax `unit_test_runner` loop guard budget

**Priority:** P1 — needed even with Change 1 for multi-step workflows.
**File:** `agents/specialists/unit_test_runner.default/SKILL.md`

```yaml
loop_guard:
  max_loops_without_progress: 4   # was 2
  max_tool_failures: 4            # was 2
  max_session_turns: 8            # was 4
```

Even with the progress fix, the runner needs enough turns to: inspect artifact
→ discover tests → exec → parse → record verdict. The current budget is too
tight for that workflow.

### Change 3: Exempt clarification sessions from divergence escalation

**Priority:** P1 — kills the clarification→diverge→clarify chain.
**Files:** `autonoetic-gateway/src/runtime/lifecycle.rs:3191-3196`,
`autonoetic-gateway/src/runtime/tool_dispatch.rs`

Clarification is a single read-only Q&A turn. It should not be subject to the
same loop-guard divergence that governs execution sessions. Two options:

**Option A (simpler):** Skip `TrajectoryMonitor::tick` entirely when
`session_state == SessionState::Clarification`. The session still has the
LoopGuard's hard limits (max_session_turns), but the sentinel will not escalate
to `Critical` divergence.

**Option B (more precise):** Override the loop guard for clarification sessions
to `max_session_turns: 1` so the executor runs exactly one turn and exits.

Recommend **Option A** — it is the lowest-risk change and preserves the existing
one-turn execution model without changing manifest parsing.

### Change 4: Make the orphan reaper notify waiting parents

**Priority:** P2 — eliminates the "stuck for 30 seconds" window.
**File:** `autonoetic-gateway/src/scheduler.rs:818-866`

When the reaper cancels a `TaskRun`, call
`task_notify.notify_session(parent_session_id)` on the `GatewayStore`'s
`TaskNotifyRegistry` so `workflow.wait` returns immediately via its signal-driven
wake path instead of waiting for the 5-second fallback poll.

### Change 5: Make `workflow.wait` detect transcript/TaskRun mismatch

**Priority:** P2 — closes the crash-window gap.
**File:** `autonoetic-gateway/src/runtime/tools/workflow.rs:570-667`

If the `TaskRun` says `Running` but the session transcript is `failed` or
`completed`, `workflow.wait` should return immediately with the failure/exit
status rather than blocking. This closes the window between transcript
finalization and TaskRun update described in section 2.5.

Implementation: in `signal_driven_wait`, after `check_task_statuses`, if any
task is still `Running`, query the session transcript for that task's
`session_id`. If the transcript is terminal, treat the task as terminal with a
`Failed` status (or `Completed` if transcript is `completed`).

---

## 4. Priority and sequencing

| # | Change | Priority | Risk | Effort |
|---|---|---|---|---|
| 1 | Decouple tool success from test exit code | P0 | Low — narrows the `failed` flag | Small |
| 2 | Relax loop guard budget | P1 | Low — manifest-only | Trivial |
| 3 | Exempt clarification from divergence | P1 | Low — skips monitor for read-only mode | Small |
| 4 | Reaper notifies waiting parents | P2 | Low — adds one notify call | Small |
| 5 | `workflow.wait` detects mismatch | P2 | Medium — adds transcript query to wait loop | Medium |

Changes 1–3 are independent and can be done in parallel. Changes 4–5 address
the stuck-session symptom and are also independent of each other.

**If only one change is done, it should be Change 1.** It directly fixes
"tests fail → session loops without concluding" by letting the runner process
failing test results as normal progress instead of being killed for diverging.

---

## 5. Test plan

### Unit tests

- `LoopGuard` + `ToolObservation`: verify that `ok: true, exit_code: 1` does
  not increment `current_loops` or `tool_failure_counts`.
- `TrajectoryMonitor`: verify that clarification sessions are not escalated to
  `Critical`.

### Integration tests

- Spawn `unit_test_runner` with an artifact whose tests fail → verify the
  session records a `fail` verdict and exits cleanly (no divergence event).
- Spawn `unit_test_runner` with an artifact whose tests pass → verify the
  session records a `pass` verdict and exits cleanly.
- Spawn a clarification child for a failed `unit_test_runner` approval → verify
  the child completes one turn and exits without divergence.
- Orphan reaper cancels a child task → verify the waiting parent's
  `workflow.wait` returns immediately (not after 5-second poll).
- Simulate transcript `failed` + TaskRun `Running` → verify `workflow.wait`
  returns failure instead of blocking.

---

## 6. Out of scope

- Generalizing the `exit_code` semantics to other agents (e.g. `executor.default`).
  This RFC targets `unit_test_runner` and `artifact_exec` specifically; other
  agents can be evaluated case-by-case.
- Redesigning the LoopGuard to distinguish "progress" from "success" at a
  semantic level (e.g. LLM-judged progress). This is a larger change that could
  be explored separately.
- Changing the clarification spawn flow to be automatic (triggered by the
  sentinel rather than the operator). Clarification remains operator-initiated.
