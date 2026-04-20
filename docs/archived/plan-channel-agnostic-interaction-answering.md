# Channel-agnostic interaction answer resolution

This document specifies how the **gateway** (not individual channel adapters) owns **persisting** `user.ask` answers and **resuming** suspended work: workflow tasks in `Paused` and standalone sessions blocked on `UserInputRequired` checkpoints.

**Related code**

- `autonoetic-gateway/src/interaction_answer.rs` — orchestration entrypoints
- `autonoetic-gateway/src/router.rs` — JSON-RPC `interaction.answer`, `interaction.resolve_and_answer`
- `autonoetic-gateway/src/runtime/tools/user_interaction.rs` — `user.ask` row creation (workflow/task bindings)
- `autonoetic-gateway/src/execution.rs` — `resume_from_user_interaction`, checkpoint resume
- `autonoetic-gateway/src/scheduler.rs` — `process_runnable_workflow_tasks` (Runnable → queue)

---

## Goals

1. **Single orchestrator**: Answer + resume is implemented once in the gateway.
2. **Dumb channels**: Messengers submit plain inbound messages + metadata; the gateway decides if the message answers a pending interaction.
3. **Deterministic resume**: After an answer, workflow-bound tasks leave `Paused` → `Runnable` and are re-queued; non-workflow sessions call the checkpoint resume path.
4. **Backward compatibility**: Existing CLIs migrate to the same orchestration (`interaction.answer` semantics), not ad-hoc store writes + `event.ingest` alone.

---

## Gateway API contracts

### `interaction.answer` (JSON-RPC)

**Purpose**: Explicit answer when `interaction_id` is known.

**Params** (`InteractionAnswerParams`):

| Field | Required | Description |
|-------|----------|-------------|
| `interaction_id` | yes | e.g. `ui-abc12345` |
| `answer_text` | one of text/option | Freeform answer (mutually exclusive with `answer_option_id`) |
| `answer_option_id` | one of text/option | Structured option id |
| `answered_by` | no | Actor label (default `gateway`) |
| `follow_up_message` | no | Optional extra user line appended after tool result injection (workflow + standalone) |

**Result** (`InteractionAnswerOutcome`): includes `answer_applied`, `resumed`, `workflow_task_unblocked`, `ambiguous`, `ambiguous_candidates`, `error`.

**Config**: `GatewayConfig.interaction_answer_orchestration` (default `true`). When `false`, the method fails fast so legacy callers can be detected during rollout.

### `interaction.resolve_and_answer` (JSON-RPC)

**Purpose**: Channel-native inbound where `interaction_id` may be missing.

**Params** (`InteractionResolveAndAnswerParams`):

| Field | Description |
|-------|-------------|
| `interaction_id` | Strongest: answer this id |
| `reply_to_interaction_id` | Second: reply threading to a prior prompt / outbound id |
| `root_session_id` | Required if neither id above: scope pending interactions |
| `answer_text` / `answer_option_id` | Same mutual exclusion as above |
| `answered_by`, `follow_up_message` | Same as explicit answer |

**Resolution order** (deterministic):

1. `interaction_id` if non-empty  
2. else `reply_to_interaction_id` if non-empty  
3. else `root_session_id` → list pending for root:
   - **0** → error  
   - **1** → auto-select  
   - **>1** → `ambiguous: true`, `ambiguous_candidates` filled; no answer persisted  

Future adapters may extend correlation (e.g. provider message maps) without changing priority order for existing clients.

---

## Persistence: `user.ask` rows

When `user.ask` runs inside a **workflow task**, the emitted `UserInteraction` row must carry:

| Column | Source |
|--------|--------|
| `workflow_id` | `NativeToolRunContext.workflow_id` |
| `task_id` | `NativeToolRunContext.task_id` |
| `checkpoint_turn_id` | Current tool `turn_id` (best-effort alignment with checkpoint turn) |

Standalone sessions (no workflow context) keep these `NULL` as before.

This enables the orchestrator to:

- Transition the correct `TaskRun` from `Paused` → `Runnable` after answer  
- Re-use the same durable queue path as approval unblocking (`process_runnable_workflow_tasks`)

---

## Post-answer resume semantics

### Workflow-bound (`workflow_id` + `task_id` present)

1. Persist answer via `GatewayStore.answer_user_interaction` (idempotent if already answered — no duplicate resume).
2. If the linked task is `Paused`:
   - Optionally refresh `TaskRun.message` from `follow_up_message` (so the queued continuation sees the user line).
   - `update_task_run_status` → `Runnable` with summary `user interaction answered; resuming task`.
   - Task checkpoint step `user_interaction_answered` (JSON payload includes `interaction_id`).
   - `process_runnable_workflow_tasks` runs so the async executor re-queues and spawns; `spawn_agent_once` auto-resumes from `UserInputRequired` when the interaction row is `answered`.

### Non-workflow (no task binding)

1. Persist answer.  
2. Call `resume_from_user_interaction(interaction_id, follow_up_message)` so the session checkpoint injects the tool result and continues the agent loop.

### Idempotency

- Duplicate answer deliveries: if the interaction is already `answered`, orchestration returns without re-running resume side-effects.

---

## Adapter migration

| Client | Before | After |
|--------|--------|-------|
| Chat TUI | `answer_user_interaction` + `event.ingest` | In-process `answer_and_orchestrate_resume` (same DB as daemon); **skips** duplicate `event.ingest` when an interaction was handled |
| `autonoetic gateway interactions answer` | Store-only answer + manual resume instructions | `answer_and_orchestrate_resume` via local `GatewayExecutionService` |
| Future WhatsApp/Telegram | — | JSON-RPC `interaction.resolve_and_answer` with `root_session_id` + metadata |

Direct `GatewayStore.answer_user_interaction` from adapters is **deprecated** for production paths; keep only for tests or transitional shims.

---

## Integration test matrix (rollout gating)

| Case | Expectation |
|------|-------------|
| `interaction.answer` by explicit `interaction_id` | Answer persisted; standalone resume or workflow Runnable per bindings |
| `interaction.resolve_and_answer` + `reply_to_interaction_id` | Resolves to same id as explicit |
| Single pending for `root_session_id` | Auto-match |
| Multiple pending for `root_session_id` | `ambiguous: true`, no mutation |
| Workflow `Paused` after `user.ask` | After answer, task `Runnable`, queue processed |
| Duplicate delivery (already answered) | Idempotent, no double resume |
| `interaction_answer_orchestration: false` | RPC error; proves flag wiring |

**Rollout**

1. Ship API + orchestration (config default **on**).  
2. Migrate chat + CLI (this repo).  
3. Monitor workflow task transitions and session checkpoint errors; disable flag only if regressions are confirmed.

---

## Risks and mitigations

| Risk | Mitigation |
|------|------------|
| Ambiguous multi-interaction threads | `ambiguous` result + explicit ids in UI |
| Partial migration (some writers still use store-only) | Document deprecation; idempotent orchestration reduces duplicate resume |
| DB contention (CLI + daemon) | Same as existing chat/store usage; prefer RPC over duplicate writers when feasible |

---

## Implementation phases (completed in tree)

1. **Bindings** — `user.ask` rows include workflow/task/checkpoint turn when run context provides them.  
2. **API** — `interaction.answer`, `interaction.resolve_and_answer`.  
3. **Orchestrator** — `interaction_answer::answer_and_orchestrate_resume`, workflow Runnable + `process_runnable_workflow_tasks`, else `resume_from_user_interaction`.  
4. **Adapters** — chat TUI + gateway CLI.  
5. **Tests / observability** — extend coverage per matrix above; rely on existing workflow + checkpoint tests where applicable.
