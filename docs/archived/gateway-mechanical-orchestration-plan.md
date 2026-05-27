# Gateway Mechanical Orchestration Plan

**Status:** Draft
**Scope:** Design only; no behavior changes in this document
**Refs:** [docs/protected-agents.md](../protected-agents.md), [docs/separation-of-powers.md](../separation-of-powers.md), [docs/workflow-orchestration.md](../workflow-orchestration.md), Constitution dumbness invariant, R++9 gateway determinism

---

## 1. Motivation

The current system pushes too much workflow control into agent prompts.

Examples:

- planners are told when to keep waiting versus re-spawn
- builders are told which error strings are transient and which are terminal
- orchestrators are told not to duplicate install work that the gateway could reject mechanically

This works as a short-term repair layer, but it has three costs:

1. **Prompt bloat**: agent instructions increasingly describe workflow mechanics instead of task semantics.
2. **String fragility**: agents classify failures by matching human-readable error text.
3. **Duplicated control logic**: the same retry and dedupe rules are repeated across planner, factory, and builder.

A particularly expensive example is manual `workflow_wait` polling logic in planner prompts. Waiting on approval, waiting on user input, deciding whether a timeout means "keep waiting" versus "escalate", and knowing that an `AwaitingApproval` task must never be cancelled are all mechanical lifecycle concerns. They should not live in prompt prose.

The result is that agents become custodians of runtime behavior that should belong to the gateway.

This violates the spirit, though not yet the letter, of the dumbness rule: the gateway should own mechanical invariants; agents should reason only about task meaning.

---

## 2. Problem Statement

There are two different kinds of logic currently mixed together:

### 2.1 Mechanical workflow logic

These are state-machine facts. They do not require agent judgment.

- whether a join condition has already been satisfied
- whether a root session should be resumed again
- whether an in-flight install is a duplicate of existing work
- whether a failure is retryable at all
- whether a retry should repeat the same stage or stop
- whether the system is waiting on an external event (approval, user input, operator action)

### 2.2 Semantic task logic

These require agent or operator judgment.

- whether the user's goal is still worth pursuing after a failure
- whether a coverage gap is acceptable
- whether to redesign an artifact
- what clarification to ask the user
- whether findings imply code repair or policy acceptance

The current system leaks too much of 2.1 into prompts.

---

## 3. Design Goal

Re-draw the boundary as follows:

- **Gateway owns execution mechanics**: typed failures, retry safety, dedupe, wake-up semantics, stage-local retry budgets.
- **Agents own semantic choice**: what to do next in the user's problem domain.

This preserves separation of powers:

- the gateway becomes stricter, not smarter
- agents become thinner, not weaker
- protected operations remain mechanically enforced

---

## 4. Non-Goals

This proposal does **not** ask the gateway to:

- decide whether the product outcome is acceptable
- redesign tasks or artifacts
- choose between business alternatives
- generate user-facing reasoning
- replace planner, agent-factory, or specialized_builder as semantic actors

The gateway remains a deterministic workflow engine with policy enforcement.

---

## 5. Design Principles

### 5.1 Mechanics belong where truth is observable

If the gateway can observe a fact directly, agents should not infer it indirectly from transcript text.

The same rule applies to failure classification: if the gateway already observed the failure boundary itself, it should classify from that observation rather than trusting an agent to self-report it.

Examples:

- workflow already `Resumable`
- task is waiting on approval
- install request duplicates active work
- revision conflict is non-retriable

### 5.2 Side-effect protection should be mechanical

Duplicate durable operations should be prevented by the gateway, not merely discouraged in prompts.

### 5.3 Retry policy should be local to the failed stage

A transient install failure should not cause the planner to recompute upstream work such as coding, packaging, or onboarding.

### 5.4 Typed outcomes beat prose

Agents should consume structured failure classes and retry advice, not error strings.

### 5.5 Suspension and wake-up are gateway concerns

If a child is blocked on approval, user input, or task completion, the parent should be resumed by the gateway on state transition. Parents should not implement polling loops as prompt logic except during migration.

---

## 6. Proposal

### 6.1 Typed failure semantics for workflow-facing tools

Any workflow-relevant tool or child-task result should expose machine-readable classification.

**Classification authority:** gateway-observed facts should take precedence over agent self-report.

Primary sources of truth include:

- tool error shape
- sandbox exit code / signal / timeout
- approval state transitions
- workflow task status transitions
- revision tool return codes and structured errors

Agent final replies may add semantic context, but the gateway should not require agents to invent or faithfully preserve mechanical `failure_class` values for the runtime to function correctly.

This avoids a fragile design where typed failure semantics depend on prompt compliance.

Suggested response shape:

```json
{
  "ok": false,
  "stage": "install",
  "failure_class": "transient_infra",
  "retry_advice": "retry_same_stage",
  "retryable": true,
  "requires_external_event": false,
  "requires_human": false,
  "side_effect_state": "unknown",
  "dedupe_key": "install:agent_id:artifact_ref",
  "summary": "model endpoint request failed"
}
```

Suggested `failure_class` values:

- `transient_infra`
- `approval_pending`
- `awaiting_user_input`
- `timeout`
- `child_cancelled`
- `artifact_invalid`
- `dependency_missing`
- `gate_unsatisfied`
- `gate_unable_to_evaluate`
- `install_conflict`
- `policy_denied`
- `schema_validation_failed`
- `task_contract_invalid`
- `unknown`

Suggested interpretation:

- `gate_unsatisfied` means a gate produced a real blocking verdict
- `gate_unable_to_evaluate` means the gate could not produce a deterministic verdict because the environment or evidence was insufficient
- `schema_validation_failed` means the payload/contract was mechanically invalid before semantic task evaluation

Suggested `retry_advice` values:

- `wait`
- `retry_same_stage`
- `retry_after_external_recovery`
- `do_not_retry`
- `escalate_human`
- `fix_artifact_then_retry`

The key property is that agents stop performing string-matching such as:

- `spawn_execute_error`
- `error sending request for url`
- `already has active revision`
- `Archived`

Those become tool/runtime classifications rather than prompt folklore.

### 6.2 Mechanical ownership of wait/suspend semantics

The gateway should own parent suspension and wake-up for child workflow state transitions.

Target behavior:

- parent delegates child work once
- if child is `approval_pending`, `awaiting_user_input`, or simply still running, the parent is suspended mechanically
- on approval resolution, user answer, cancellation, timeout, or terminal completion, the gateway wakes the parent with typed state
- the parent does not poll `workflow_wait` in a loop to rediscover state the gateway already knows

This does not remove `workflow_wait` as a tool. It changes its role:

- **current role**: primary polling primitive for orchestration
- **target role**: explicit inspection/debugging primitive and compatibility shim during migration

This is effectively a Phase 0 ergonomics change because it removes a large fraction of prompt-level orchestration boilerplate without requiring semantic gateway logic.

### 6.3 Stage-local retry budgets in workflow state

The gateway should track retry budget per stage instance instead of making agents remember policies.

**Constitutional compliance (I-4, §14 dumbness):** Automatic stage-local retry is a recovery decision. To comply with I-4, the retry policy must be **opt-in per manifest or workflow configuration**. A workflow or task declares its retry policy (e.g. `retry_policy: { transient_infra: { max_retries: 1 } }`). Absent an explicit policy, the gateway does not retry — it returns the failure to the parent agent for semantic decision. This mirrors the existing R-4.11 `credential_refresh` exception: a declared, bounded, mechanical action, not a silent gateway judgment.

Suggested default policy (when declared):

- `transient_infra` -> retry the **same stage** once
- `approval_pending` -> wait only
- `awaiting_user_input` -> wait only
- `timeout` -> retry or escalate based on stage-local budget and child state
- `child_cancelled` -> stop and surface cancellation mechanically
- `install_conflict` -> stop immediately
- `artifact_invalid` -> return control to parent agent
- `schema_validation_failed` -> stop and route to parent for input/output contract repair
- `gate_unable_to_evaluate` -> stop and surface as coverage/environment gap, not artifact rejection
- `dependency_missing` -> allow deterministic packager handoff
- repeated identical failure on same dedupe key -> escalate and stop

This avoids a common failure mode where a late install failure causes the parent to re-run coder, researcher, or onboarding unnecessarily.

Budget exhaustion should be explicit gateway behavior:

- mark the task `Failed`
- set `retry_advice: "do_not_retry"`
- emit a workflow event describing budget exhaustion and last failure class

### 6.4 Failure propagation across delegation chains

Typed failure metadata should survive wrapping across delegation layers.

Example:

- `specialized_builder` returns `failure_class: transient_infra` at stage `install`
- `agent-factory` may add local context (`agent_id`, `artifact_ref`, parent stage name)
- `agent-factory` must **not** silently reclassify it into a generic prose error
- `planner` receives the same underlying `failure_class` and can rely on gateway-normalized `retry_advice`

Rule:

- gateway-native tools and runtime boundaries classify the local failure they observed
- wrapper agents may enrich context
- when an agent reports a failure class that disagrees with gateway-observed facts, gateway-observed facts win for mechanical policy
- wrapper agents should not reinterpret a mechanical failure class unless they are introducing a strictly more specific sub-class

This preserves determinism across delegation boundaries.

### 6.5 Single-flight protection for durable operations

The gateway should coalesce or reject duplicate durable work in progress.

Primary target operations:

- install
- promote
- rollback
- artifact-backed build stages that create durable side effects

Candidate single-flight key:

- `(workflow_id, stage_kind, agent_id, artifact_ref)`

For reasoning-only installs, replace `artifact_ref` with a normalized intent digest.

If a duplicate request arrives while one is active, the gateway may return:

```json
{
  "ok": true,
  "status": "coalesced",
  "existing_task_id": "task-123",
  "retry_advice": "wait"
}
```

This makes duplicate suppression a runtime guarantee rather than a planner habit.

### 6.6 Side-effect state classification

`side_effect_state` should be explicit rather than illustrative.

Suggested values:

- `none` — no durable side effect has occurred yet; retry is mechanically safe
- `committed` — the side effect happened; dedupe and do not retry blindly
- `unknown` — the process failed across an uncertain boundary; reconciliation may be required

This matters for install/promotion/replacement flows where the distinction between "nothing happened" and "something may already have happened" determines whether retry is safe.

### 6.7 Idempotent workflow wake-up semantics

Workflow wake/resume should be a transition, not a repeated notification.

Required invariant:

- `workflow.join.satisfied` is emitted once per join epoch
- root-session continuation is scheduled once per newly-satisfied join
- later terminal task updates do not re-open the same join

This is already the right conceptual model for workflow truth and should be explicit gateway behavior.

### 6.8 Structured revision conflict results

Revision tools should classify non-retriable install-state conflicts explicitly.

Suggested `install_conflict` subcodes:

- `active_revision_exists`
- `revision_archived`
- `rollback_lineage_mismatch`
- `dedup_points_to_archived_revision`
- `alias_missing_after_replace`

These should come directly from `agent_revision_create_from_intent` and `agent_revision_promote` rather than being reconstructed from free text in prompts.

### 6.9 Relationship to existing LoopGuard

This proposal overlaps with, but does not replace, LoopGuard.

Proposed boundary:

- **LoopGuard**: intra-turn protection against repeated tool failures or loops without progress
- **Stage-local retry budget**: inter-turn workflow policy for whether a stage may be retried again after suspension, resume, or later wake-up

LoopGuard remains a local execution safety device. Stage-local retry budget becomes the workflow-level policy engine.

The two should compose, not compete.

---

## 7. Boundary: What Stays in Agents

Even after this design, agents still decide:

- whether to continue pursuing the user's goal
- whether a failing evaluator means the artifact is wrong or the acceptance bar changed
- whether to ask the user for clarification
- whether to redesign or rewrite an artifact
- whether an operator should be consulted for product-level tradeoffs

In short:

- gateway answers: "what mechanically happened?"
- agents answer: "what should this mean for the task?"

---

## 8. Impact on Existing Agents

If adopted, prompts for the following agents should shrink substantially:

- planner
- agent-factory
- specialized_builder
- evaluator-family agents where applicable

Expected simplifications:

- fewer paragraphs about waiting vs retrying
- elimination of manual `workflow_wait` polling loops in normal orchestration
- fewer handwritten stop conditions
- no prompt-level parsing of infrastructure failures
- less duplicated install-conflict handling

The prompts should reduce to: consume `workflow_state`, respect `retry_advice`, then make the next semantic choice.

---

## 9. Migration Plan

### Phase 0: Mechanical wake-up semantics

Introduce typed child-state notifications and parent wake-up on state transitions so prompt-level polling loops become unnecessary.

### Phase 1: Typed outcome plumbing

Add `failure_class`, `retry_advice`, and related fields to workflow/task/tool results without changing orchestration behavior yet.

Emit both typed fields and legacy human-readable messages during the transition period.

### Phase 2: Install-stage single-flight

Add duplicate suppression for durable install/promotion work.

### Phase 3: Stage-local retry policy

Move bounded retry logic into workflow state and scheduler behavior.

### Phase 4: Prompt simplification

Remove redundant retry/error-management prose from planner, agent-factory, and specialized_builder.

Keep backward-compatible message text for one release cycle after typed fields ship. During that period, prompts may consume typed fields preferentially while still tolerating legacy strings.

### Phase 5: Broader workflow adoption

Extend the same semantics to other durable operations such as escalation-driven resumptions and post-promotion reviews where appropriate.

---

## 10. Tradeoffs

### Benefits

- thinner agent prompts
- less duplicated control logic
- fewer accidental duplicate side effects
- less brittle string matching
- easier testing at the gateway layer

### Costs

- more structured result plumbing in the gateway
- a slightly richer workflow state model
- migration work to keep old prompts functioning during transition

### Risk

If overdone, the gateway could start encoding semantic policy rather than workflow policy.

The safeguard is simple: the gateway may classify, dedupe, wait, retry, or stop mechanically, but it must not decide business intent.

---

## 11. Decisions and Resolved Questions

### 11.1 Provisional decisions

1. **`retry_advice` should be normalized centrally by the workflow layer.**
Tools should emit `failure_class` plus local context. The workflow layer should compute `retry_advice`, because only it knows stage-local retry budget, prior failures, and whether waiting is still valid.

2. **Single-flight should surface coalescing explicitly.**
The parent agent should be told that work was coalesced so it can wait rather than redesign or restart upstream work.

3. **Failure class should be pass-through across delegation boundaries.**
Wrapping agents may enrich context, but should not erase the underlying mechanical classification.

4. **Gateway-observed classification is authoritative.**
Typed failure semantics should be derived from gateway-observed boundaries where possible, not delegated to agent self-report. Agent replies remain useful for semantic context, but runtime retry/dedupe/wait policy should rely on what the gateway directly observed.

### 11.2 Resolved questions

1. **Should the retry budget live on the task row, the workflow row, or a per-stage sub-structure?**

   **Decision: task row.** A `TaskRun` already maps 1:1 to a single stage execution (one `agent_id`, one `session_id`, one logical operation). Adding `retry_count: u32`, `last_failure_class: Option<String>`, and `retry_policy: Option<serde_json::Value>` directly to `TaskRun` is the simplest correct place. A per-stage sub-structure would require a new join table and add complexity without benefit — retry is always scoped to a specific task's execution, not to the workflow as a whole. The workflow row is too coarse (a workflow has many stages). The task row is already the unit of work for scheduling, status transitions, and persistence.

2. **How should gateway-native wake-up semantics coexist with explicit `workflow_wait` during migration?**

   **Decision: dual-path with `workflow_wait` becoming a no-op after native wake-up.** During migration:
   - Gateway-native wake-up schedules parent resumption automatically when a child reaches terminal state or resolves a gate.
   - If the parent is currently blocked on `workflow_wait`, the wake-up satisfies the wait (same as today, but triggered by state transition rather than polling).
   - If the parent hasn't called `workflow_wait` yet (new agents post-migration), the wake-up injects typed child state directly into the parent's context at the next turn boundary.
   - `workflow_wait` remains functional but becomes semantically equivalent to "assert I am waiting for this child" — useful for debugging, but not required for correct orchestration.
   - Post-migration (Phase 4+), prompts that still call `workflow_wait` continue to work. The tool is not removed, just demoted from primary orchestration primitive to inspection/debugging tool.

3. **Should revision tools emit a generic `install_conflict` plus subcode, or more specific top-level failure classes?**

   **Decision: generic `install_conflict` plus subcode.** All install conflicts share the same workflow-level behavior: non-retriable, stop immediately, do not re-run coder or builder. Making each subcode a top-level `failure_class` would bloat the enum with domain-specific values that all map to `retry_advice: do_not_retry`. The subcode field (`install_conflict_detail: active_revision_exists | revision_archived | ...`) preserves full diagnostic detail for agents that need it, without polluting the workflow policy engine. Agents that care about *why* an install failed read the subcode; agents that only need to know *what to do next* read `failure_class: install_conflict` + `retry_advice: do_not_retry`.

4. **How much compatibility shimming is needed for prompts that still expect prose-only failure output?**

   **Decision: one full release cycle of dual emission, then prose-only path removed.** Phase 1 emits both typed fields (`failure_class`, `retry_advice`, `side_effect_state`) and legacy fields (`error_type`, `message`, `repair_hint`) in every workflow-relevant tool response. Old prompts read prose fields; new prompts read typed fields. No conditional logic — just redundancy. Phase 4 removes prompt-level prose parsing and the legacy fields are deprecated (but not removed from the response shape to avoid breaking external consumers). This matches the migration pattern used for R++1 state attestation (old prompts relied on tool output for budget state; new prompts read the signed attestation block).

---

## 12. Constitutional Amendments Required

This section lists new constitutional rules and amendments needed to support the proposal. Each follows the amendment process: rule text, target section, enforcement mechanism, and test requirement.

### 12.1 New pending rules (R+)

| ID | Rule | Target | Enforcement | Rationale |
|---|---|---|---|---|
| R+19 | **Mechanical failure classification.** Every workflow-relevant tool failure is classified into a `failure_class` from a closed enum. Classification is a pure function of gateway-observed state (sandbox exit codes, tool error shapes, approval state, workflow task status). Agents may add semantic context but may not override gateway-observed classification for mechanical policy purposes. | §5 I/O Schema | `runtime/tool_call_processor.rs` or `runtime/lifecycle.rs` classification layer; `constitution_r_5_14_mechanical_failure_classification.rs` | Extends R-5.11 uniform error envelope. Without this rule, agents must string-match error prose, which is the core problem this plan addresses. Classification authority must be gateway-observable to be deterministic (R++9). |
| R+20 | **Single-flight protection for durable operations.** Duplicate durable operations (install, promote, rollback, artifact-backed build stages) are detected by `(workflow_id, stage_kind, agent_id, artifact_ref)`. Duplicate requests while one is active return `status: coalesced` with `retry_advice: wait`. This extends approval dedup (R-2.3) to all durable side-effecting operations. | §2 Approval Gates / §6 Workflow | `scheduler/single_flight.rs` or equivalent; `constitution_r_6_24_single_flight_protection.rs` | R-2.3 deduplicates approvals. This extends the same guarantee to install/promote/rollback, which currently rely on prompt-level "do not duplicate" instructions. |
| R+21 | **Stage-local retry budget.** Workflow-bound tasks track per-stage retry counts against a declared policy. Budget exhaustion marks the task `Failed` with `retry_advice: do_not_retry` and emits a `stage_budget_exhausted` causal event. Retry policy is opt-in per manifest or workflow configuration (I-4 compliance); absent explicit policy, failures return to the parent agent without automatic retry. | §6 Workflow | `workflow_store` retry tracking + `scheduler.rs` budget check; `constitution_r_6_25_stage_local_retry_budget.rs` | Separates inter-turn retry policy from intra-turn LoopGuard (R-7.5). Makes retry a declared, testable configuration rather than agent prompt folklore. |
| R+22 | **Side-effect state tracking.** Durable operations report `side_effect_state` from a closed enum (`none`, `committed`, `unknown`). Retry and dedupe decisions MUST consult side-effect state. Retrying a `committed` operation is a policy violation. | §6 Workflow | revision/install tool responses; `constitution_r_6_26_side_effect_state.rs` | Without this, agents cannot distinguish "nothing happened" from "something may already have happened," leading to either duplicate side effects or unnecessary upstream rework. |

### 12.2 New rights (Ri-0)

| ID | Right | Target | Enforcement | Rationale |
|---|---|---|---|---|
| Ri-0.14 | **Mechanical parent wake-up.** When a child task reaches a terminal state (completed, failed, cancelled) or resolves a gate (approval, user interaction, escalation), the parent is woken with typed child state. Parents are not required to poll to discover child state transitions. `workflow_wait` remains available as an inspection/debugging tool. | §0 Rights | `scheduler.rs` wake-up on task/gate state transitions; `constitution_right_ri_0_14.rs` | Currently parents implement polling loops as prompt logic (~20 lines in planner alone). This is a mechanical lifecycle concern the gateway already knows about. Making it a right ensures no future implementation can regress to requiring parent polling for correct orchestration. |

### 12.3 Amendments to existing rules

| Rule | Change | Reason |
|---|---|---|
| R-5.11 | Extend uniform error envelope to include `failure_class`, `retry_advice`, `side_effect_state`, and `dedupe_key` for workflow-relevant tool failures. | Current envelope is `{error_type, message, repair_hint}`. The plan adds structured classification fields that agents and the workflow layer consume for mechanical decisions. |
| R-6.13 | New yield reasons for mechanical parent suspension: `WaitingForChild` (parent suspended while child is running or gate-pending). Checkpoints must cover the new yield reasons. | Mechanical wake-up introduces a new suspension path where the parent is suspended because a child is in progress. This must be a declared yield reason per Ri-0.12's closed list. |
| Ri-0.12 | Add `WaitingForChild` to the closed list of session yield causes (6 terminal + 5 resumable). | Ri-0.12 requires all termination/suspension reasons to be in a closed list. Mechanical parent wake-up adds a new resumable cause. |
| I-4 | Add explicit exception: "Stage-local retry of mechanically-identical operations (same tool, same arguments, same stage) is permitted when the task's declared retry policy allows it. This is not a recovery decision — it is policy execution against a declared budget." | Current I-4 says the gateway does not make recovery decisions. Declared, bounded, mechanical retry is policy execution, not judgment. The exception mirrors the existing R-4.11 `credential_refresh` pattern. |

### 12.4 Relationship to existing rules (no change required)

These existing rules support the plan and require no amendment:

| Rule | How it relates |
|---|---|
| R-2.3 | Approval dedup is the existing precedent for single-flight (R+20). The new rule extends the same principle to durable operations. |
| R-5.11 | The uniform error envelope is the foundation for typed failure classification (R+19). The amendment is additive. |
| R-6.13 | Checkpoint coverage already requires all yield reasons. Ri-0.14 and the Ri-0.12 amendment add the new `WaitingForChild` yield reason, which R-6.13 then covers by existing mechanism. |
| R-7.5 (LoopGuard) | Intra-turn per-tool failure counting. Stage-local retry (R+21) is inter-turn and per-stage. The two compose per Section 6.9. |
| R++9 | Gateway determinism test suite. All new classification, retry, and dedupe behavior must be pure functions of declared state — no LLM calls, no network fetches, no hidden branches. |
| Ri-0.3 | Every rejection names the rule ID. The plan's `policy_denied` failure class must carry rule IDs through the existing `Tagged::permission_with_rules` path. |

### 12.5 Constitutional dependency order

This is dependency order for ratification and implementation, not numeric phase order.

1. **Phase 1**: R+19 (failure classification) — additive, no behavior change
2. **Phase 0**: Ri-0.14 (mechanical wake-up) + Ri-0.12/R-6.13 amendments (new yield reasons)
3. **Phase 2**: R+20 (single-flight) + R+22 (side-effect state)
4. **Phase 3**: R+21 (stage-local retry) + I-4 amendment
5. **Phase 4**: R-5.11 amendment (extend envelope) + prompt simplification

---

## 13. Recommendation

Adopt this design incrementally.

The minimum high-value move is:

1. typed failure classes
2. mechanical wake-up semantics for child suspension/resolution
3. idempotent wake-up transitions
4. single-flight protection for install stages

That gets most of the benefit without turning the gateway into a semantic planner.

The guiding rule should be:

> Agents choose the next meaningful step. The gateway guarantees that workflow mechanics are safe, typed, and non-duplicative.
