# RFC: Unit Test Runner Divergence Loop & Stuck-Session Recovery

**Status:** Implemented — 2026-06-23. All five changes landed (Changes 1–5).
See §7.
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

### 2.1 The failure signal originates in `artifact_exec`, and two paths consume it

There are **two independent** failure-detection paths in the turn loop, and they
read different fields — but both ultimately key on what `artifact_exec` reports.

**What `artifact_exec` returns.** On every completed run
(`artifact_exec.rs:957`, `:1284`; mirrored in `sandbox.rs:2085`):

```rust
let ok = output.status.success();
... "ok": ok, "exit_code": output.status.code(), ...
```

`ok` is *defined as* `exit_code == 0`. So when a test suite legitimately fails,
`artifact_exec` returns `{"ok": false, "exit_code": 1, ...}`. There is no
`ok: true, exit_code != 0` case — `ok` and the exit code carry the same signal.

**Path A — the LoopGuard (the actual session-killer).** In
`lifecycle.rs:3091-3150`:

- `parsed["ok"] == false` → `self.guard.register_failure(...)` → increments
  `tool_failure_counts["artifact_exec"]` toward `max_tool_failures`.
- otherwise → `tool_result_counts_as_progress()` (`tool_dispatch.rs:66`), which
  checks `ok` **first** (`return ok;`) and only falls back to `exit_code == 0`
  when there is no `ok` field.

Because `artifact_exec` always emits `ok: false` on a failing suite, **every
failing test run takes the `register_failure` branch** and counts as a tool
failure. With `max_tool_failures: 2`, two failing runs trip the guard. This —
not the trajectory-monitor block below — is the mechanism that kills the runner.

**Path B — the trajectory monitor.** Separately, `lifecycle.rs:3167-3189`
builds `ToolObservation`s for `TrajectoryMonitor::tick`:

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

For a failing suite (`ok: false`) this is `failed = true` via the first check
already; the `exit_code` clause is redundant in practice for `artifact_exec`.

**Root cause.** Both paths inherit the same category error, but it is rooted in
`artifact_exec` itself: `ok = status.success()` conflates *the sandbox executed
the command* with *the command exited 0*. For a test runner those are different
facts — a suite that runs and reports failures is a **correct, successful tool
execution** that produced a verdict the runner must act on. Editing only the
trajectory-monitor `failed` computation (as an earlier framing of Change 1
proposed) would not help: that block is Path B, and it reads `ok: false`
regardless of the `exit_code` clause; the session-killer is Path A, driven by
`artifact_exec`'s `ok` field. The fix must change what `artifact_exec` reports.

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

### Change 1: Make `artifact_exec` report tool-execution success, not command exit status

**Priority:** P0 — this is the root-cause fix.
**Files:** `autonoetic-gateway/src/runtime/tools/artifact_exec.rs:957-968` (and
the mirrored finalizer at `:1284`),
`autonoetic-gateway/src/runtime/lifecycle.rs:3173-3183`.

Redefine `ok` for `artifact_exec` to mean **the sandbox ran the command to
completion without a sandbox-level malfunction**, decoupled from the command's
exit code:

```rust
// artifact_exec.rs — replace `let ok = output.status.success();`
let exit_code = output.status.code();
let command_succeeded = output.status.success();
// `ok` = the tool worked: the sandbox ran the command to completion with an
// exit code in the normal range (0–127). A non-zero exit code here is a DOMAIN
// result the runner must process (tests failed), not a tool failure. A signal
// kill (no exit code) or any signal-derived code (>= 128: SIGKILL/OOM 137,
// SIGTERM 143, SIGSYS/seccomp 159) is a sandbox-level fault and stays ok:false.
let ok = matches!(exit_code, Some(code) if (0..128).contains(&code));
let mut body = serde_json::json!({
    "ok": ok,
    "command_succeeded": command_succeeded,
    "exit_code": exit_code,
    // ... stdout, stderr, artifact_ref, entrypoint as today
});
```

> **Why an exit-code gate, not `detect_sandbox_escape_indicators`?** The escape
> detector matches the substring `"permission denied"` (its EACCES/seccomp
> heuristic). A benign test that prints "permission denied" would wrongly flip
> `ok` back to `false`. Gating on the exit code (ran to completion, not
> SIGSYS-killed) has no false positives on test output.

Genuine tool failures already set `ok: false` before this point (missing
artifact, spawn error) or override it afterward
(`apply_network_isolation_failure_to_result` sets `ok: false` /
`approval_required` on a network deny). Those paths are unchanged. Only the
"ran fine, exited non-zero" case flips from `ok: false` to `ok: true`.

With this change **Path A reconverges on the correct behavior for free**:
`lifecycle.rs:3092` no longer takes the `register_failure` branch, and
`tool_result_counts_as_progress` returns `ok` (true) → the run registers as
progress. No edit to the LoopGuard or `tool_dispatch.rs` is needed.

**Path B still needs a one-line fix.** The trajectory observation at
`lifecycle.rs:3173-3183` treats `exit_code != 0` as `failed` even when
`ok == true`. Drop the secondary `exit_code` clause so it keys only on `ok`:

```rust
let failed = parsed.as_ref().map_or(false, |v| {
    v.get("ok").and_then(|o| o.as_bool()) == Some(false)
});
```

| Condition (from `artifact_exec`) | Before | After |
|---|---|---|
| sandbox malfunction (SIGSYS, escape, spawn fail) | `ok:false` → failure | **unchanged** — real tool failure |
| ran, tests failed (`exit_code != 0`) | `ok:false` → failure + no progress | **`ok:true` → success + progress** |
| ran, tests passed (`exit_code == 0`) | `ok:true` → success + progress | **unchanged** |

**Verify the verdict still reflects failure.** `promotion_record` derives `pass`
from the `execution_trace` (its exit code), not from the tool's `ok` flag, and
the runner's own status mapping reads `exit_code`/test output. Confirm in
implementation that a failing suite still yields a `fail` verdict — `ok: true`
means "the gate ran," not "the artifact passed." The new `command_succeeded`
field is the explicit domain signal for any consumer that previously leaned on
`ok` to mean "exited 0."

**Scope.** This changes `artifact_exec` only. `sandbox_exec` (used by
`executor.default`) keeps `ok = status.success()` for now — see §6.

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
| 1 | `artifact_exec` reports tool success, not exit code | P0 | Low–Med — changes `ok` semantics for `artifact_exec`; verify verdict logic | Small |
| 2 | Relax loop guard budget | P1 | Low — manifest-only | Trivial |
| 3 | Exempt clarification from divergence | P1 | Low — skips monitor for read-only mode | Small |
| 4 | Reaper notifies waiting parents | P2 | Low — adds one notify call | Small |
| 5 | `workflow.wait` detects mismatch | P2 | Medium — adds transcript query to wait loop | Medium |

Changes 1–3 are independent and can be done in parallel. Changes 4–5 address
the stuck-session symptom and are also independent of each other.

**If only one change is done, it should be Change 1.** It removes the root
category error so the runner processes failing suites as normal progress instead
of being killed for diverging. If you need to stop the production bleeding
*immediately* with zero code risk, **Change 2 is a stopgap**: raising
`max_tool_failures`/`max_session_turns` enlarges the failure budget the current
`ok:false` accounting consumes, keeping the runner alive through a failing suite
until Change 1 lands. Change 1 is the correct fix; Change 2 alone only buys
headroom.

---

## 5. Test plan

### Unit tests

- `artifact_exec`: a command that runs cleanly but exits non-zero returns
  `ok: true, command_succeeded: false, exit_code: 1`; a SIGSYS/escape run still
  returns `ok: false`.
- `LoopGuard` + `tool_result_counts_as_progress`: an `artifact_exec` result with
  `ok: true, exit_code: 1` registers as progress and does **not** increment
  `tool_failure_counts`.
- `TrajectoryMonitor`: an observation with `ok: true, exit_code: 1` is not
  `failed`; and clarification sessions are not escalated to `Critical`.

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

- **Generalizing the `ok`/`exit_code` decoupling to `sandbox_exec`.** Change 1
  deliberately redefines `ok` for `artifact_exec` only. `sandbox_exec` (used by
  `executor.default` and others) keeps `ok = status.success()`: a generic
  executor re-running a failing command *should* still feed the loop-guard's
  failure accounting, and flipping it risks masking genuinely stuck build/command
  loops. Whether the same decoupling helps those agents is a separate,
  case-by-case evaluation.
- Redesigning the LoopGuard to distinguish "progress" from "success" at a
  semantic level (e.g. LLM-judged progress). This is a larger change that could
  be explored separately.
- Changing the clarification spawn flow to be automatic (triggered by the
  sentinel rather than the operator). Clarification remains operator-initiated.

---

## 7. Implementation status

| # | Change | Status |
|---|---|---|
| 1 | `artifact_exec` reports tool success, not exit code | ✅ Done |
| 2 | Relax `unit_test_runner` loop guard budget | ✅ Done |
| 3 | Exempt clarification from divergence | ✅ Done |
| 4 | Reaper notifies waiting parents | ✅ Done |
| 5 | `workflow.wait` detects transcript/TaskRun mismatch | ✅ Done |

**Change 1** — `artifact_exec.rs` (both finalizers) now sets
`ok = matches!(exit_code, Some(code) if (0..128).contains(&code))` plus a new
`command_succeeded` field; the trajectory observation in `lifecycle.rs:3173`
keys only on `ok`. The gate accepts the normal exit range (0–127) as a domain
result; a signal kill (no exit code) or any signal-derived code (≥ 128 —
SIGKILL/OOM 137, SIGTERM 143, SIGSYS/seccomp 159) stays `ok: false`, so repeated
OOM/timeout kills are not mistaken for progress. Verdict safety is preserved by
`trace_indicates_pass`'s `exit_code == 0` gate and locked in by
`promotion_evidence::tests::trace_with_nonzero_exit_is_fail_even_when_success_flag_set`.

**Observability:** `infer_trace_success` (`tool_call_processor.rs`) now prefers
`command_succeeded` over `ok`, so the recorded `ExecutionTraceRecord.success`
(and digest/overview surfaces derived from it) reflect the actual command
outcome rather than merely that the sandbox executed. A failing suite records
`success: 0, exit_code: 1`; the promotion verdict (gated on `exit_code == 0`)
is unchanged.

**Change 2** — `unit_test_runner.default/SKILL.md` loop guard raised to
`max_loops_without_progress: 4`, `max_tool_failures: 4`, `max_session_turns: 8`.

**Change 3** — the trajectory-monitor block in `lifecycle.rs` is now a labeled
block (`'trajectory_monitor`) that `break`s immediately when
`session_state == SessionState::Clarification` (Option A). The LoopGuard's hard
limits still bound clarification turns; only divergence escalation is skipped.

**Change 4** — `scheduler.rs` orphan reaper now calls
`store.task_notify.notify_session(&task.parent_session_id)` after cancelling a
child `TaskRun`, waking a parent blocked in `workflow.wait` via its
signal-driven path instead of its 5-second fallback poll.

**Change 5** — `signal_driven_wait` (`workflow.rs`) extends the post-grace stall
check: a still-`Running` task whose session transcript is terminal is reconciled
(`failed`/`aborted`/`cancelled`/`error` → `Failed`; `completed` → `Succeeded`),
join completion is recomputed from the reconciled view, and the wait returns
immediately instead of blocking to `timeout_secs`. The pre-existing
no-transcript stall case is preserved.

### Test status

- `cargo build -p autonoetic-gateway`: clean.
- `cargo test -p autonoetic-gateway --lib`: **1375 passed, 0 failed**.
- Two integration tests fail on `main` **independently of this work** (verified
  by stashing these changes and re-running on the clean tree):
  `trajectory_monitor_integration::looping_session_produces_divergence_event_sequence`
  and `workflow_wait_signal_driven::sequential_transitions_both_complete_fast`.
  These are pre-existing failures, not regressions from this RFC.

### Follow-up tests worth adding (§5 integration coverage, not yet written)

- `unit_test_runner` against a failing-test artifact → `fail` verdict, no
  divergence event, clean exit.
- Clarification child that loops on read-only tools → completes without a
  `Critical` divergence escalation.
- Orphan reaper cancels a child → waiting parent's `workflow.wait` returns
  promptly (not after the 5s fallback).
- Transcript `failed` + TaskRun `Running` past the grace period →
  `workflow.wait` returns a failed join immediately.
