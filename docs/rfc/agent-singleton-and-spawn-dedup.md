# RFC: Singleton Agents and Deterministic `agent_spawn` Semantics

**Status:** Draft — 2026-06-29
**Authors:** OpenCode session, from observed session `session-79b128af`
**Related:** `docs/AGENTS.md`, `docs/rfc/gateway-agent-divergence-robustness.md`, `autonoetic-gateway/src/execution.rs`, `autonoetic-gateway/src/scheduler/workflow_store.rs`

---

## 1. Summary

This RFC proposes a small set of gateway-side primitives that make agent coordination deterministic and cheap, without asking every SKILL.md to become a concurrency expert.

The core ideas are:

1. **Declare certain agent roles as singletons** in their manifest. A singleton can have at most one pending or running task per workflow.
2. **Make `agent_spawn` idempotent by default for singletons.** A second call returns the existing task/session instead of starting a duplicate.
3. **Route duplicate spawns as messages** when the singleton is already running and the caller wants to hand it a follow-up task.
4. **Block new spawns and suppress stale notifications once a workflow is `Completed`.**

The goal is to stop the retry/backpressure/notification loops that burned ~80 turns and several hundred thousand tokens in the `fibonacci.calculator` install session.

---

## 2. Motivation

### 2.1 What we observed

In session `session-79b128af` the planner tried to install a `fibonacci.calculator` agent. The trace shows:

- Three separate `agent-factory.default` sessions were spawned because the first one appeared stuck.
- Each factory tried to smoke-test the candidate by spawning `fibonacci.calculator`.
- All those spawns hit `Backpressure: pending execution queue is full for agent 'fibonacci.calculator'`.
- The factories and planner kept retrying, creating more spawns, which made backpressure worse.
- After the agent was finally installed and promoted, stale `workflow.child.resolved` and join-satisfied signals kept waking the planner, which spawned `fibonacci.calculator` again to "verify" it was working.
- The log ends with the planner at `turn_counter=12`, still cycling.

Key log lines:

```
Backpressure: pending execution queue is full for agent 'fibonacci.calculator'
Task force-completed ... new_status=Failed
No alias 'fibonacci.calculator' found — the agent has not been promoted yet
Reaping orphaned session ... agent-factory.default
Resuming session ... turn_counter=3/4/5/.../12 yield_reason=Hibernation
```

### 2.2 Why this keeps happening

LLM agents are stateless reasoners. When they wake up, they do not reliably remember that a spawn is already in flight. The current `agent_spawn` contract is "fire and forget"; the gateway will create as many tasks as the agent asks for. That is fine for worker agents, but it is catastrophic for coordinator/reviewer/installer roles, where running two copies in parallel is almost never useful and frequently harmful.

SKILL.md files already contain many ad-hoc rules to prevent this: "do not respawn a completed step", "call `workflow_state` first", "trust a child step's terminal result". Those rules help, but they are hard for an LLM to follow reliably. The right place to enforce "only one architect at a time" is the gateway, not the prompt.

---

## 3. Goals and Non-Goals

### 3.1 Goals

- Eliminate duplicate parallel instances of coordinator/reviewer/installer agents within a single workflow.
- Eliminate the backpressure storms caused by those duplicates competing for per-agent admission semaphores.
- Stop completed workflows from being woken up by stale child-state notifications.
- Keep agent logic simple: agents can call `agent_spawn` whenever they want; the gateway provides coordination.
- Preserve the ability to run worker agents in parallel when that is genuinely intended.

### 3.2 Non-Goals

- We are not removing parallelism globally. We are making it opt-in where it matters.
- We are not replacing `workflow_wait` or `workflow_state`. We are reducing the need for agents to poll them defensively.
- We are not adding a general message-equivalence detector. We dedup on identity, not on task content.

---

## 4. Proposed Changes

### 4.1 Agent manifest: `singleton` flag

Add a boolean field to the `metadata.autonoetic.agent` block in SKILL.md:

```yaml
metadata:
  autonoetic:
    agent:
      id: "architect.default"
      singleton: true   # default false
```

A singleton agent can have **at most one pending or running task per `(root_session_id, workflow_id, agent_id)`**. If a spawn request arrives while one is already in flight, the gateway returns the existing task/session instead of creating a new one.

The flag is a property of the **agent role**, not of an individual spawn. The agent author decides once whether this role is parallelizable.

### 4.2 `agent_spawn` behavior for singletons

For a singleton agent:

| Existing state | `agent_spawn(...)` result |
|---|---|
| No task exists | Create task/session normally. |
| Task pending or running | Return existing task/session metadata; do not create a duplicate. |
| Task terminal (completed/failed) | Create a new task/session. Singletons serialize across time but are not one-shot. |

The response shape remains the same, but includes a marker so the caller can tell what happened:

```json
{
  "ok": true,
  "task_id": "task-xxx",
  "session_id": "session-xxx/...",
  "agent_id": "architect.default",
  "singleton": true,
  "deduplicated": false
}
```

When deduplicated, `deduplicated: true` and `existing_task_id` / `existing_session_id` point to the in-flight work.

### 4.3 Opt-out: `allow_duplicate: true`

Callers that genuinely need a second concurrent instance of a singleton can pass `allow_duplicate: true`:

```json
{
  "agent_id": "architect.default",
  "message": "...",
  "async": true,
  "allow_duplicate": true
}
```

This is an explicit, auditable override. It should be rare and is intended for advanced cases (e.g., two independent design explorations in the same workflow).

### 4.4 Message routing for duplicate spawns

Sometimes a caller wants to give a running singleton a **new task** rather than wait for the current one. We support two modes:

**A. `queue_if_running: true`**

```json
{
  "agent_id": "unit_test_runner.default",
  "message": "Also run the integration artifact tests.",
  "async": true,
  "queue_if_running": true
}
```

If the singleton is already running, the spawn is converted to an `agent_message` to the existing session and the caller receives the existing task id. The running agent handles the follow-up in its normal turn loop.

**B. Return `agent_already_running`**

If neither `allow_duplicate` nor `queue_if_running` is set, and a singleton is running, the gateway returns the existing task id. The caller is expected to `workflow_wait` on it.

### 4.5 Workflow completion guard

Once a workflow reaches status `Completed`, the gateway must:

1. Reject new `agent_spawn` calls that target that workflow with `workflow_already_completed`.
2. Drop in-flight `workflow.child.resolved` and `WorkflowJoinSatisfied` notifications instead of delivering them to the root session.
3. Reject `workflow.force_complete` and `workflow.amend` calls on completed workflows.

This stops the post-install wake loop where stale notifications keep resuming a planner that has already emitted its final response.

### 4.6 Gate admission semaphore per workflow

The `max_pending_spawns_per_agent` semaphore currently keys on `agent_id` globally. We propose scoping the singleton admission check to `(workflow_id, agent_id)` so that concurrent workflows are not coupled. This matches the semantics of "one architect per project", not "one architect across the whole gateway".

---

## 5. Singleton Classification

We classify the built-in agents as follows. This list should live in `docs/AGENTS.md` and be enforced by manifests.

### Singletons (coordinators, reviewers, installers)

| Agent | Why singleton |
|---|---|
| `planner.collaborative` / `planner.default` | One lead per root session. |
| `architect.default` | Design is serial; two architects on the same artifact waste tokens and conflict. |
| `packager.default` | Packaging the same artifact twice in parallel is redundant. |
| `agent-factory.default` | One install pipeline at a time per target agent. |
| `specialized_builder.default` | One installer per root session. |
| `auditor.default` | Audit verdict should be one coherent result. |
| `static_evaluator.default` | Static review is deterministic; parallel copies add no value. |
| `unit_test_runner.default` | Test run is a single verdict per artifact. |
| `sealed_evaluator.default` | Same as unit_test_runner. |
| `debugger.default` | One debug investigation at a time. |
| `outcome-grader.default` | One grading pass per artifact. |
| `registration.default` | Credential ceremonies should be serialized. |
| `improvement-orchestrator.default` | Orchestrator role. |
| `watchdog.default` / `watchdog-fast.default` | One watchdog per workflow. |
| `memory-curator.default` | One curator pass at a time. |
| `evolution-steward.default` | One steward decision at a time. |

### Non-singletons (worker agents)

| Agent | Why parallelizable |
|---|---|
| `coder.default` | Can code different files/modules in parallel. |
| `executor.default` | Can run independent commands in parallel. |
| `researcher.default` | Can research independent topics in parallel. |
| `discovery.default` | Can search different intents in parallel. |
| User-installed worker agents (e.g., `fibonacci.calculator`) | Often instantiated multiple times for independent tasks. |

### Special case: user-facing worker agents

Agents installed via `agent-factory.default` should generally **not** be singletons unless the install intent explicitly sets `singleton: true`. A calculator, weather agent, or API client is expected to handle many concurrent invocations.

---

## 6. Backwards Compatibility

- Existing agents without `singleton: true` behave exactly as before.
- `allow_duplicate: true` lets callers opt out of singleton behavior.
- The new `deduplicated` / `singleton` fields in the spawn response are additive.
- The workflow completion guard only affects workflows that have already reached `Completed`; it does not change behavior of active workflows.

There is one behavioral change for existing callers: if they currently rely on spawning e.g. `architect.default` twice in the same workflow to get two parallel design explorations, they must now pass `allow_duplicate: true`. We believe this is extremely rare and undesirable.

---

## 7. Implementation Notes

### 7.1 Data model

Add `singleton: bool` to `AgentManifest` in `autonoetic-types`.

In the workflow store, maintain a per-workflow index of in-flight singleton tasks. On `agent_spawn`:

1. Resolve agent manifest.
2. If `singleton: true` and no `allow_duplicate: true`:
   - Look up `(workflow_id, agent_id)` in the in-flight index.
   - If found and task is pending/running, return existing task.
   - If `queue_if_running: true`, additionally send `agent_message` to the existing session.
3. Otherwise, create new task as today.

### 7.2 Workflow completion guard

In `update_task_run_status` / workflow state transitions:

- When the workflow transitions to `Completed`, clear pending notifications for that workflow and mark them `suppressed`.
- In the JSON-RPC router, reject `agent_spawn` calls whose `workflow_id` (explicit or inferred from session) is completed.
- Reject `workflow.force_complete`, `workflow.amend`, `workflow_wait` on completed workflows.

### 7.3 Admission semaphore

Scope the existing `agent_admission_semaphore` in `execution.rs:execute_with_reliability_controls` to `(workflow_id, agent_id)` for singleton agents. Non-singleton agents keep the global agent-id semaphore.

### 7.4 SKILL.md updates

- Add `singleton: true` to each singleton manifest.
- Remove or simplify the ad-hoc "do not respawn" / "call workflow_state first" rules in planner/factory/builder SKILLs once the gateway enforces the behavior.

---

## 8. Relation to Observed Failures

| Observed failure | How this RFC fixes it |
|---|---|
| Planner spawned 3 `agent-factory.default` instances | `agent-factory.default` is a singleton; spawns 2 and 3 return the first factory's task id. |
| Factories spawned `fibonacci.calculator` repeatedly for smoke test | For a non-singleton, this is allowed, but the workflow completion guard prevents post-install retries. Better: factory's own retry logic becomes a single `workflow_wait` on one smoke-test task. |
| Backpressure on `fibonacci.calculator` admission queue | Singleton admission per workflow + fewer duplicate spawns reduces queue pressure. |
| Force-completed tasks and stale child-state notifications kept waking planner | Workflow completion guard suppresses notifications after `Completed`. |
| Post-install verification spawns | Rejected once workflow is `Completed`. |

---

## 9. Open Questions

1. Should `singleton` be a **capability** instead of a manifest flag? A capability would let it be gated by policy; a manifest flag is simpler.
2. Should `agent_spawn` dedup consider `revision_id`? Two spawns of the same singleton with **different** `revision_id`s are arguably different work. We suggest deduping on `agent_id` only, unless `revision_id` differs and the caller passes `allow_duplicate: true`.
3. Should we expose a "current task id" query tool (`workflow.singleton_status`) so agents can discover in-flight singletons without attempting a spawn?
4. How does this interact with `agent_message`? If a singleton is running and a caller sends `agent_message`, it is naturally routed to the existing session. We may not need `queue_if_running` at all if `agent_message` is the idiomatic follow-up.

---

## 10. Recommended Next Steps

1. Add `singleton: bool` to `AgentManifest` and parser.
2. Implement per-workflow singleton task index in the workflow store.
3. Update `agent_spawn` to respect singleton semantics and return `deduplicated` marker.
4. Implement workflow completion guard.
5. Mark built-in singleton agents in their SKILL.md manifests.
6. Add integration tests:
   - Second spawn of singleton returns existing task.
   - `allow_duplicate: true` bypasses singleton dedup.
   - Completed workflow rejects new spawns.
   - Stale notifications are suppressed.
7. Simplify planner/factory SKILLs after the gateway enforces the behavior.
