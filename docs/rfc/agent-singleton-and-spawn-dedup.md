# RFC: Singleton Agents and Deterministic `agent_spawn` Semantics

**Status:** Draft — 2026-06-29 (rev. 2)
**Authors:** OpenCode session, from observed session `session-79b128af`
**Related:** `docs/AGENTS.md`, `docs/rfc/gateway-agent-divergence-robustness.md`, `autonoetic-gateway/src/execution.rs`, `autonoetic-gateway/src/scheduler/workflow_store.rs`

---

## 1. Summary

This RFC proposes gateway-side primitives that make agent coordination deterministic and cheap, without asking every SKILL.md to become a concurrency expert.

Three layers, in priority order:

1. **Workflow completion guard** — once a workflow is terminal, reject new spawns and suppress stale notifications. This is a standalone bug fix; ship first.
2. **Singleton agents** — declare coordinator/reviewer/installer roles as singletons in their manifest. The gateway allows at most one pending or running task per singleton per workflow. Duplicate `agent_spawn` calls return the existing task instead of creating a parallel session.
3. **Stateful singleton sessions** (phase 2) — a singleton's session persists across tasks. Subsequent spawns become messages, reusing loaded context. This saves system-prompt and context-assembly tokens but adds lifecycle complexity; defer until the simpler layers are proven.

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

### 2.3 The token cost

Each redundant `agent_spawn` of a singleton pays the full system-prompt cost (~15–18k tokens for most specialists) plus context assembly, even if the duplicate is immediately backpressured or force-completed. In the observed session:

| Redundant spawns | System-prompt tokens wasted |
|---|---|
| 2 extra `agent-factory.default` instances | ~36k |
| Repeated auditor / static_evaluator / unit_test_runner batches | ~90k |
| Post-install `fibonacci.calculator` verification spawns | ~15k |

Singleton dedup (phase 1) eliminates the duplicate sessions entirely. Stateful singleton sessions (phase 2) additionally eliminates re-loading the system prompt for the same agent across sequential tasks.

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
- Phase 2 (stateful singleton sessions) is explicitly deferred; it is not a blocker for shipping phases 0 and 1.

---

## 4. Phase 0: Workflow Completion Guard

> **Ship this first.** It is a standalone bug fix, independent of singletons.

### 4.1 The problem

Once a workflow reaches `Completed`, in-flight `workflow.child.resolved` and `WorkflowJoinSatisfied` notifications continue to wake the root session. The planner resumes, sees nothing to do, emits a summary, and hibernates — only to be woken again by the next stale notification. In the observed session this loop ran for 10+ extra minutes.

### 4.2 The fix

Once a workflow reaches a terminal status (`Completed`, `Failed`, `Cancelled`, `EmergencyStopped`):

1. **Reject new `agent_spawn` calls** that target that workflow. Return `workflow_already_completed` with the terminal status and a pointer to the existing task results.
2. **Suppress in-flight notifications** for that workflow — mark pending `workflow.child.resolved` and `WorkflowJoinSatisfied` notifications as `suppressed` instead of delivering them.
3. **Reject workflow mutations** — `workflow.force_complete` returns `workflow_already_completed`. Plan amendment (`planframe_amend`) already rejects non-mutable plan statuses independently. `workflow_wait` remains allowed on terminal workflows so agents can read final task results.

The completion check must happen **inside `agent_spawn`**, not only in the workflow state machine. In the observed session, new tasks were enqueued during a turn whose processing also marked the workflow completed — the spawn succeeded because the completion check ran after, not before.

### 4.3 Atomicity

The completion check and the task creation must be in the same locked section of the workflow store. Without this, a race window allows a spawn to sneak in between "workflow completing" and "workflow completed".

---

## 5. Phase 1: Singleton Agents

### 5.1 Agent manifest: `singleton` flag

Add a boolean field to the `metadata.autonoetic.agent` block in SKILL.md:

```yaml
metadata:
  autonoetic:
    agent:
      id: "architect.default"
      singleton: true   # default false
```

A singleton agent can have **at most one pending or running task per `(workflow_id, agent_id, revision_id)`** within a workflow. If a spawn request arrives while one is already in flight, the gateway returns the existing task/session instead of creating a duplicate.

The flag is a property of the **agent role**, not of an individual spawn. The agent author decides once whether this role is parallelizable.

### 5.2 Dedup key: `(workflow_id, agent_id, revision_id)`

The dedup key includes `revision_id` when the caller provides one. Two spawns of the same singleton with **different** revision_ids are genuinely different work (e.g., smoke-testing revision A vs. revision B). Spawns with the same revision_id, or both without one, are treated as duplicates.

### 5.3 `agent_spawn` behavior for singletons

| Existing state | `agent_spawn(async=true)` result | `agent_spawn(async=false)` result |
|---|---|---|
| No task exists | Create task/session normally. | Create and block as today. |
| Task pending or running | Return existing task/session with `deduplicated: true`. | **Block** on the existing task; return its result when done. |
| Task terminal | Create a new task/session. | Create a new task and block. |

The response includes a marker so the caller can tell what happened:

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

When deduplicated, `deduplicated: true` and the `task_id` / `session_id` point to the in-flight work. The caller can `workflow_wait` on the returned `task_id`.

For **sync spawns** (`async: false`): the gateway blocks on the existing task and returns its result. This is what the caller expects from a sync call — a result, not a "go look elsewhere" marker.

### 5.4 No `allow_duplicate` in v1

The original draft proposed an `allow_duplicate: true` escape hatch. We are **dropping it from v1**. If an agent is declared singleton, the point is "don't run two in parallel." If someone genuinely needs two concurrent architects, they should either wait for the first to finish, use a separate root session, or change the manifest flag. Adding an escape hatch that LLMs might over-use ("I'll set allow_duplicate=true to be safe") undermines the guarantee.

We can add it later if a concrete use case emerges.

### 5.5 No `queue_if_running` — use `agent_message` explicitly

The original draft proposed automatic conversion of duplicate spawns into messages. We are **dropping this from v1**. Silently converting a spawn to a message changes the contract: the caller expected a fresh session; it gets a message routed into a session with different context. The result shape is unpredictable.

Instead: if the singleton is running and the caller wants to send a follow-up task, the caller uses `agent_message` explicitly. This keeps the two operations distinct and the LLM can understand each one.

### 5.6 Atomic check-and-create

Two parents (planner + factory) can both call `agent_spawn` for the same singleton in the same millisecond. The workflow store must perform an **atomic check-and-create**: look up the singleton index and create the task if absent, in a single locked transaction. Without this, the race window allows duplicates.

### 5.7 Singleton task timeout

If a singleton task is stuck in `Running` forever (the agent hung, the LLM provider is down, etc.), the singleton constraint blocks all future spawns of that agent for the workflow. There must be a **timeout**: after N minutes with no checkpoint progress (default: 10), the gateway marks the singleton task as `Failed` with reason `singleton_timeout`, releasing the singleton slot so a new spawn can replace it.

This is especially important for `agent-factory.default` — if the factory hangs, nothing else can install the agent.

### 5.8 Emergency stop cleanup

Emergency stop cancels all tasks in a root session. The singleton index must be cleaned up atomically with the emergency stop. Otherwise the index retains stale "running" entries that block future workflows from spawning the same singleton.

### 5.9 Admission semaphore scoping

The `max_pending_spawns_per_agent` semaphore currently keys on `agent_id` globally. For singleton agents, the dedup mechanism already prevents queue saturation within a workflow, so the semaphore change is less critical. We keep the global semaphore as-is for v1; it serves as a cross-workflow safety net.

---

## 6. How the Observed Failures Are Fixed

| Observed failure | Fix | Phase |
|---|---|---|
| Planner spawned 3 `agent-factory.default` instances | `agent-factory.default` is a singleton; spawns 2 and 3 return the first factory's task_id. | 1 |
| Each factory spawned `fibonacci.calculator` for smoke test, hitting backpressure | Only one factory runs → only one smoke-test spawn chain. `fibonacci.calculator` is NOT a singleton, but the cascading spawns were caused by the duplicate factories. With one factory, there is one smoke-test task. | 1 (indirect) |
| Planner directly spawned `fibonacci.calculator` in parallel with the factory | Planner's spawn of `fibonacci.calculator` is not deduped (it's not a singleton), but the planner gets the factory's singleton task_id back and `workflow_wait`s on it instead of spawning the target directly. | 1 (indirect) |
| Force-completed tasks emitted child-state notifications | Completion guard suppresses notifications after workflow is terminal. | 0 |
| Post-install `fibonacci.calculator` verification loop | Completion guard rejects new spawns once workflow is `Completed`. | 0 |
| Planner resumed 10+ times after final response | Completion guard suppresses stale `WorkflowJoinSatisfied` signals. | 0 |

**Key insight:** `fibonacci.calculator` itself does not need to be a singleton. The cascade was caused by duplicate singletons upstream (factories, planner coordination). Fixing the singletons eliminates the cascade.

---

## 7. Singleton Classification

We classify the built-in agents as follows.

### Singletons

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

### Non-singletons (parallelizable workers)

| Agent | Why parallelizable |
|---|---|
| `coder.default` | Can code different files/modules in parallel. |
| `executor.default` | Can run independent commands in parallel. |
| `researcher.default` | Can research independent topics in parallel. |
| `discovery.default` | Can search different intents in parallel. |
| User-installed worker agents (e.g., `fibonacci.calculator`) | Often instantiated multiple times for independent tasks. |

### Special case: user-facing worker agents

Agents installed via `agent-factory.default` should **not** be singletons unless the install intent explicitly sets `singleton: true`. A calculator, weather agent, or API client is expected to handle many concurrent invocations.

---

## 8. Phase 2: Stateful Singleton Sessions (Deferred)

> This section analyzes a future optimization. It is **not** part of the v1 implementation.

### 8.1 The idea

A singleton agent gets **one long-lived session per workflow**. The first `agent_spawn` creates the session. Subsequent spawns for the same singleton are converted to `agent_message` calls to that session, reusing the loaded system prompt and accumulated context.

This is distinct from the existing `type: "stateful"` manifest field, which means the agent can persist data to durable storage across sessions. A stateful singleton session persists the **LLM conversation** itself.

### 8.2 Token savings

In the observed session, the federation gates (auditor, static_evaluator, unit_test_runner) were each spawned twice — once per quality-gate batch. Each spawn paid the full system-prompt cost (~15–18k tokens). With a persistent session, the second invocation would be a ~500-token message into the existing session. Across three gates and three factory instances, this saves an estimated **90–130k tokens** per install session.

For comparison, phase-1 singleton dedup (preventing duplicate sessions entirely) saves the ~36k tokens from the two extra factory instances. Phase 2 saves the additional context-reload cost for legitimate sequential tasks within the same singleton.

### 8.3 Why it is more complicated

| Concern | Detail |
|---|---|
| **Context contamination** | An auditor that reviewed artifact v1 and found issues might be biased when re-reviewing the fixed v2 in the same session. Fresh sessions are more objective. The contamination risk is highest for evaluators and auditors; lowest for architects and planners. |
| **Session lifecycle** | When does the persistent session end? If it lives for the whole workflow, context accumulates. For long workflows (50+ turns), the context window fills up. The gateway would need to compact or summarize mid-session, which is lossy and hard to get right. |
| **"Task done" vs "session done"** | Currently, agents return a final JSON and the session ends. A persistent singleton needs a way to say "this task is done, ready for the next one" without terminating the session. This is a new concept in the agent contract — a "yield for next task" yield reason distinct from "end turn." |
| **Error propagation** | If the persistent session develops a wrong assumption (misread a file, hallucinated an API), all subsequent tasks in that session inherit the error. Fresh sessions are more resilient because each starts from a clean state. |
| **Observability** | A persistent session that handles multiple tasks produces a longer, harder-to-read transcript. Debugging "which task caused the wrong assumption" requires correlation across task boundaries within the same session log. |

### 8.4 When it would be worth it

Stateful singleton sessions make sense when:

- The agent's tasks are **closely related** (same codebase, same install pipeline, same audit target with minor revisions). Shared context genuinely helps.
- The agent is **not an evaluator** (contamination risk is low). Architects, planners, and factories benefit; auditors and evaluators do not.
- The workflow is **short enough** that context accumulation is manageable (< 20 tasks per singleton).

### 8.5 Recommendation

Defer phase 2 until phase 0 + phase 1 are shipped and measured. If phase 1 eliminates the duplicate-session tokens (the majority of the waste), the remaining context-reload savings may not justify the complexity. If measurement shows that sequential singleton invocations still consume significant tokens on system-prompt reloads, revisit with a focused proposal.

If pursued, start with `architect.default` and `agent-factory.default` (low contamination risk, high context-reuse benefit). Do NOT start with evaluators/auditors.

---

## 9. Backwards Compatibility

- Existing agents without `singleton: true` behave exactly as before.
- The new `deduplicated` / `singleton` fields in the spawn response are additive.
- The workflow completion guard only affects workflows that have already reached a terminal status; it does not change behavior of active workflows.

One behavioral change for existing callers: if a caller currently spawns a singleton-declared agent twice in the same workflow, the second spawn returns the first task instead of creating a parallel session. This is the intended behavior; callers that need the result should `workflow_wait` on the returned task.

---

## 10. Implementation Notes

### 10.1 Phase 0: Workflow completion guard

In the workflow store and JSON-RPC router:

1. Add a `is_workflow_terminal(workflow_id) -> bool` check.
2. In `agent_spawn` handler: check terminal status **before** task creation, inside the same lock as task creation.
3. In notification pump (`scheduler.rs:deliver_pending_signals`): skip notifications whose workflow is terminal; mark them `suppressed`.
4. In `workflow.force_complete`, `workflow.amend`, `workflow_wait`: return `workflow_already_completed` error.

### 10.2 Phase 1: Singleton data model

Add `singleton: bool` to `AgentManifest` in `autonoetic-types`.

In the workflow store, maintain a per-workflow index: `HashMap<(workflow_id, agent_id, Option<revision_id>), task_id>`. On `agent_spawn`:

1. Resolve agent manifest.
2. If `singleton: true`:
   - Acquire workflow-store lock.
   - Look up `(workflow_id, agent_id, revision_id)` in the index.
   - If found and task is pending/running → return existing task with `deduplicated: true`.
   - Otherwise → create new task, insert into index, release lock.
3. On task terminal transition → remove from index.
4. On emergency stop → clear all index entries for the root session.

### 10.3 Phase 1: Singleton timeout

Background sweeper (already exists for orphan reaping): every tick, scan singleton tasks that have been `Running` with no checkpoint update for > `singleton_timeout_secs` (default 600). Mark as `Failed` with `singleton_timeout`.

### 10.4 SKILL.md updates — what becomes unnecessary

After the gateway enforces singleton behavior, these specific SKILL rules become mechanically enforced and can be simplified:

| Current SKILL rule | Where | After singleton enforcement |
|---|---|---|
| "Trust a child step's terminal result; don't re-spawn to confirm it" | `planner.collaborative` §Resumption | Mechanically enforced for singleton children (dedup returns existing/terminal task). |
| "Do not spawn parallel factories" | `planner.collaborative` (implicit) | Enforced: `agent-factory.default` is a singleton. |
| "Check `reuse_guards.has_builder_candidate` before creating" | `agent-factory.default` Step 5 | Partially replaced: singleton dedup prevents duplicate builder spawns. Still useful for cross-resume state. |
| "After 2 retries on the same stage: report failure" | `agent-factory.default` §Error Handling | Still needed — retry after failure is legitimate; singleton prevents parallel, not sequential retries. |

Rules that remain necessary (singleton does NOT address these):
- "Do not call `agent_list` repeatedly" (roster polling, not spawn dedup).
- "Call `workflow_state` on resume" (checkpoint recovery, not spawn dedup).
- "Process all child results in one turn after `workflow_wait`" (turn batching, not spawn dedup).

---

## 11. Open Questions

1. **Capability vs manifest flag.** Should `singleton` be a capability (gated by policy) or a manifest field (purely declarative)? Current recommendation: manifest field. Capabilities gate what an agent *can do*; singleton declares what the agent *is*.

2. **Scope: per-workflow vs per-root-session.** Should the singleton constraint be `(workflow_id, agent_id)` or `(root_session_id, agent_id)`? For most agents per-workflow is fine. For `agent-factory.default`, per-root-session might be better (two factories installing different agents in the same session still compete for the single installer). Open: measure whether this matters in practice.

3. **Singleton status query.** Should we expose a `workflow.singleton_status` tool so agents can discover in-flight singletons without attempting a spawn? Probably yes — it helps the planner decide whether to wait or proceed.

4. **Phase 2 trigger.** What metric determines whether phase 2 (stateful sessions) is worth building? Candidate: "system-prompt tokens reloaded for sequential singleton invocations per workflow." If this exceeds 50k on average, revisit.

---

## 12. Recommended Implementation Order

| Step | What | Why first |
|---|---|---|
| 1 | Workflow completion guard (phase 0) | Standalone bug fix; highest impact; no manifest changes needed. |
| 2 | Integration tests for completion guard | Verify no regressions in active-workflow spawns. |
| 3 | `singleton: bool` in `AgentManifest` + parser | Foundation for phase 1. |
| 4 | Per-workflow singleton index + atomic check-and-create | Core dedup mechanism. |
| 5 | Update `agent_spawn` to respect singleton semantics | Return `deduplicated` marker; sync spawn blocks. |
| 6 | Singleton timeout sweeper | Prevent stuck singletons from blocking workflows. |
| 7 | Emergency stop cleanup of singleton index | Safety. |
| 8 | Mark built-in singleton agents in SKILL.md manifests | Enable the behavior for coordinators/reviewers/installers. |
| 9 | Simplify planner/factory SKILLs | Remove rules now mechanically enforced. |
| 10 | Measure token/turn impact; decide on phase 2 | Data-driven decision. |
