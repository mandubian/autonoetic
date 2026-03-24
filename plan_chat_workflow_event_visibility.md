## Plan: Fix Chat Workflow Event Visibility

Stabilize event delivery to chat by eliminating SQLite/JSONL divergence in workflow event reads/writes, then harden chat-side incremental event tracking and redraw signaling so approval-resume transitions are always visible.

**Steps**
1. Phase 1 - Event source consistency (blocking)
2. In `load_workflow_events`, remove early-return behavior that prefers any non-empty SQLite result and never checks JSONL; instead define a deterministic strategy (single-source or merge with dedupe by `event_id`).
3. In `append_workflow_event`, stop swallowing store write failures (`let _ = ...`); propagate or log and expose errors clearly so data path failures are visible. Depends on step 2.
4. Add/adjust tests for store/file divergence scenarios where SQLite misses new events but JSONL contains them; verify readers still return newest events. Depends on steps 2-3.
5. Phase 2 - Chat polling and bootstrap robustness (depends on phase 1)
6. In chat bootstrap, mark all fetched event IDs as seen (or switch to timestamp cursor) while still rendering only a recap subset; avoid partial dedupe state from recap window only.
7. Ensure `check_signals` reports UI changes when approval/waiting state shrinks (not only when new approvals/events appear), so status line updates are not suppressed.
8. Optionally introduce `load_workflow_events_since` usage in chat to avoid full list rescans every second and reduce stale-snapshot effects. Parallel with step 7 after step 4.
9. Phase 3 - Observability and regression safety
10. Add targeted debug logs/metrics for: event append attempts, store/file counts, and last-seen event cursor in chat.
11. Add an integration test: approval required -> approval accepted -> task Runnable/Running -> visible chat signals and status updates.

**Relevant files**
- `/home/mandubian/workspaces/mandubian/ccos/autonoetic/autonoetic-gateway/src/scheduler/workflow_store.rs` — event append/load policy (`load_workflow_events`, `append_workflow_event`, `update_task_run_status`).
- `/home/mandubian/workspaces/mandubian/ccos/autonoetic/autonoetic-gateway/src/scheduler/gateway_store.rs` — store query behavior (`list_workflow_events`, `list_workflow_events_since`).
- `/home/mandubian/workspaces/mandubian/ccos/autonoetic/autonoetic/src/cli/chat.rs` — polling loop, bootstrap dedupe, redraw gating (`run_loop`, `check_signals`).
- `/home/mandubian/workspaces/mandubian/ccos/autonoetic/autonoetic-gateway/src/scheduler/approval.rs` — approval-to-runnable transition (`unblock_task_on_approval`).

**Verification**
1. Run gateway + chat with a workflow requiring approval; confirm sequence appears in chat: awaiting approval -> approval accepted -> resumed/running/completed signals.
2. Execute Rust tests for scheduler and chat-related behavior in `autonoetic` workspace, including newly added divergence and resume visibility tests.
3. Validate no silent store-write failures in logs during approval/resume path.
4. Restart chat mid-workflow and verify recap appears once, then only incremental live events appear without duplicates.

**Decisions**
- Include: fixing event source consistency and chat visibility gaps for approval resume path.
- Exclude: changing planner semantics or task execution behavior unrelated to event transport/display.
- Keep existing event names unless tests prove ambiguity; prefer transport/state fixes first.

**Further Considerations**
1. Choose long-term source of truth: Option A SQLite-only (recommended), Option B JSONL-only, Option C merge mode with migration window.
2. Decide whether to surface explicit `task.resumed` event type later for UX clarity, after transport correctness is restored.
