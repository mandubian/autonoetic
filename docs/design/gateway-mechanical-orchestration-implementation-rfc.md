# Gateway Mechanical Orchestration — Implementation RFC

**Status:** Implemented RFC
**Depends on:** [../archived/gateway-mechanical-orchestration-plan.md](../archived/gateway-mechanical-orchestration-plan.md)
**Scope:** Implementation plan only; no code changes in this document
**Refs:** [../workflow-orchestration.md](../workflow-orchestration.md), [../separation-of-powers.md](../separation-of-powers.md), [../protected-agents.md](../protected-agents.md), [human-gate-unification-plan.md](./human-gate-unification-plan.md)

---

## 1. Objective

Implement gateway-owned workflow mechanics so agents no longer carry prompt-level logic for:

- failure classification
- retry safety
- duplicate durable-operation suppression
- parent wake-up on child state transitions
- install-conflict stop conditions

The target outcome is:

- gateway determines mechanical workflow truth
- agents consume typed workflow facts and make semantic decisions
- existing prompts continue to function during migration

This RFC turns the design plan into concrete types, modules, rollout phases, and test gates.

---

## 2. Non-Goals

This RFC does **not** implement:

- semantic planner policy
- artifact redesign decisions
- operator-facing product judgment
- replacement of planner, agent-factory, or specialized_builder
- removal of `workflow_wait`

`workflow_wait` remains available throughout migration and beyond, but it stops being the primary orchestration primitive.

---

## 3. Implementation Principles

1. **Gateway-observed facts are authoritative for mechanical policy.**
2. **Typed fields are additive first.** No behavior flips in the first rollout slice.
3. **Parent wake-up is event-driven, not poll-driven.**
4. **Retry is declared policy, not silent gateway judgment.**
5. **Compatibility is explicit.** Old prompts keep working for at least one release cycle.

---

## 4. Deliverables

### 4.1 Typed workflow result model

Add closed enums and envelope fields for:

- `failure_class`
- `retry_advice`
- `side_effect_state`
- `dedupe_key`

Implementation note: these fields extend the existing `ToolError` wire format rather than introducing a second top-level error wrapper.

### 4.2 Native child-state wake-up path

Add a gateway-driven parent wake/resume path for:

- child terminal completion
- approval resolution
- user-input resolution
- cancellation
- timeout

### 4.3 Single-flight dedupe for durable operations

Add duplicate suppression/coalescing for:

- install
- promote
- rollback
- artifact-backed durable build stages

### 4.4 Stage-local retry budget

Track retry metadata on the task row and normalize `retry_advice` in the workflow layer.

### 4.5 Compatibility shim

Continue emitting legacy prose fields while typed fields roll out.

---

## 5. Data Model Changes

### 5.1 New enums

Add to `autonoetic-types`:

```rust
pub enum FailureClass {
    TransientInfra,
    ApprovalPending,
    AwaitingUserInput,
    Timeout,
    ChildCancelled,
    ArtifactInvalid,
    DependencyMissing,
    GateUnsatisfied,
    GateUnableToEvaluate,
    InstallConflict,
    PolicyDenied,
    SchemaValidationFailed,
    TaskContractInvalid,
    Unknown,
}

pub enum RetryAdvice {
    Wait,
    RetrySameStage,
    RetryAfterExternalRecovery,
    DoNotRetry,
    EscalateHuman,
    FixArtifactThenRetry,
}

pub enum SideEffectState {
  #[serde(rename = "none")]
  NoSideEffect,
    Committed,
    Unknown,
}
```

Serde should use stable snake_case strings. The wire value remains `"none"`, but the Rust enum variant should not shadow `Option::None`.

### 5.2 Workflow failure envelope

Do **not** introduce a separate `WorkflowFailureEnvelope` wrapper in the wire format. Instead, extend the existing `ToolError` shape directly for workflow-relevant tool failures:

```rust
pub struct ToolError {
  // existing fields...
    pub failure_class: Option<FailureClass>,
    pub retry_advice: Option<RetryAdvice>,
    pub retryable: Option<bool>,
    pub requires_external_event: Option<bool>,
    pub requires_human: Option<bool>,
    pub side_effect_state: Option<SideEffectState>,
    pub dedupe_key: Option<String>,
}
```

This is the canonical failure carrier for tool errors. Non-error workflow payloads may reuse the same field names, but they should not wrap `ToolError` in an additional envelope.

### 5.3 `TaskRun` additions

Add fields to the persisted task row:

```rust
pub struct TaskRun {
    // existing fields...
    pub retry_count: u32,
    pub last_failure_class: Option<FailureClass>,
    pub retry_policy: Option<serde_json::Value>,
    pub side_effect_state: Option<SideEffectState>,
    pub dedupe_key: Option<String>,
}
```

`retry_policy` is stored on the task row because retry is scoped to a single logical stage execution.

Current repository note: `TaskRun` records are file-backed JSON under the workflow store, so these fields land via serde-compatible defaults on the persisted task documents rather than a SQLite `task_runs` migration. If task rows move into SQLite later, the equivalent schema/backfill work belongs in [autonoetic-gateway/src/scheduler/gateway_store/migrate.rs](../autonoetic-gateway/src/scheduler/gateway_store/migrate.rs).

If task persistence moves into SQLite, the required migration work is:

- increment `SCHEMA_VERSION_LATEST`
- add a new `apply_*_vN()` migration function
- backfill defaults for existing task rows

### 5.4 New workflow event types

Add event names:

- `workflow.child.waiting`
- `workflow.child.resolved`
- `workflow.stage_budget_exhausted`
- `workflow.single_flight.coalesced`

These should appear in workflow events and session digest summaries.

### 5.5 Optional typed child-state payload

Add a typed resume payload for parent resumption:

```rust
pub struct ChildStateNotification {
    pub workflow_id: String,
    pub task_id: String,
    pub child_session_id: String,
    pub child_status: String,
    pub failure_class: Option<FailureClass>,
  pub install_conflict_detail: Option<String>,
    pub retry_advice: Option<RetryAdvice>,
    pub side_effect_state: Option<SideEffectState>,
    pub summary: Option<String>,
}
```

Delivery mechanism:

- if the parent is blocked on `workflow_wait`, deliver this as the synthetic resolution payload that satisfies that wait
- otherwise, deliver it via scheduler signal/resume plumbing and inject it into parent turn-start context alongside existing gateway state/context blocks
- do not deliver it as free-text prose; it must remain structured context

This is the structured replacement for parent polling loops.

---

## 6. Classification Authority and Precedence

Mechanical classification must be derived from gateway-observed state first.

### 6.1 Precedence order

1. Native tool structured error/result
2. Runtime-observed boundary
   - sandbox timeout
   - process signal / non-zero exit
   - approval transition
   - workflow task cancellation
   - task timeout
3. Structured child task metadata
4. Agent final reply fields
5. Fallback `Unknown`

### 6.2 Canonical mapping examples

| Observation | `failure_class` | `retry_advice` basis |
|---|---|---|
| Sandbox timed out | `timeout` | workflow-layer policy |
| Tool returned approval required | `approval_pending` | always `Wait` |
| Child task cancelled | `child_cancelled` | always `DoNotRetry` unless new spawn is explicit |
| Revision tool says archived/active revision exists | `install_conflict` | always `DoNotRetry` |
| Tool payload violates schema | `schema_validation_failed` | parent repair path |
| Gate reports environment insufficient | `gate_unable_to_evaluate` | stop; semantic escalation path |
| Connection refused / 5xx / transport reset | `transient_infra` | workflow-layer retry budget |

### 6.3 Agent self-report treatment

Agent replies may add:

- `summary`
- domain context
- artifact-specific or operator-facing explanation

Agent replies may not override gateway-observed classification for retry, dedupe, or wake-up policy.

---

## 7. Module Plan

### 7.1 `autonoetic-types`

Primary files:

- `autonoetic-types/src/workflow.rs`
- `autonoetic-types/src/tool_error.rs`
- `autonoetic-types/src/session.rs` if yield reasons live there

Changes:

- add new enums and typed envelope fields
- add `WaitingForChild` yield cause
- preserve backward-compatible deserialization where existing payloads omit new fields

### 7.2 Failure classification layer

New gateway module:

- `autonoetic-gateway/src/runtime/failure_classification.rs`

Responsibilities:

- map native tool/runtime observations to `FailureClass`
- infer `SideEffectState` where possible
- populate the workflow-specific fields on `ToolError` and sibling typed payloads
- never call the LLM

Input sources:

- tool execution results
- sandbox metadata
- `GateService::check` results from `runtime/human_gate.rs`
- workflow task status changes
- revision tool responses
- approval/user-interaction resolution state

Primary hook points:

- `autonoetic-gateway/src/runtime/tool_call_processor.rs` after each direct tool result/error
- `autonoetic-gateway/src/scheduler/workflow_store.rs` when child task status becomes terminal or suspended
- `autonoetic-gateway/src/runtime/human_gate.rs` when a gate is cleared, suspended, or deduped

### 7.3 Workflow retry policy layer

Likely location:

- `autonoetic-gateway/src/runtime/tools/workflow.rs`
- `autonoetic-gateway/src/scheduler/workflow_store.rs`
- `autonoetic-gateway/src/scheduler/gateway_store/workflow.rs`
- `autonoetic-gateway/src/scheduler/gateway_store/migrate.rs`

Responsibilities:

- normalize `retry_advice` from `failure_class + retry_count + retry_policy`
- persist retry metadata on task rows
- emit budget-exhaustion workflow events
- own persistence compatibility/backfill for the new task fields (currently JSON task documents; SQLite migration only if task rows move into the database)

### 7.4 Single-flight layer

New module:

- `autonoetic-gateway/src/scheduler/single_flight.rs`

Responsibilities:

- compute dedupe key
- check for active equivalent durable operation
- return coalesced result when appropriate
- emit `workflow.single_flight.coalesced`
- clear stale reservations during scheduler tick after task completion/failure cleanup

### 7.5 Parent wake-up layer

Likely touch points:

- `autonoetic-gateway/src/scheduler.rs`
- `autonoetic-gateway/src/scheduler/signal.rs`
- `autonoetic-gateway/src/runtime/lifecycle.rs`
- `autonoetic-gateway/src/scheduler/workflow_store.rs`

Responsibilities:

- suspend parent on `WaitingForChild`
- resume parent when child transitions to relevant state
- satisfy legacy `workflow_wait` if present
- inject typed child-state payload for new agents
- use scheduler signal delivery rather than ad hoc prompt text injection

### 7.6 Tool-specific wiring

Target first-wave tools:

- workflow tooling
- agent revision tooling
- sandbox exec path
- approval/user-interaction resume path

Later waves can adopt the same envelope progressively.

### 7.7 Scheduler tick integration

The existing scheduler tick in `autonoetic-gateway/src/scheduler.rs` remains the reconciliation point for this RFC.

Required integrations:

- cleanup of stale single-flight reservations for completed/failed tasks
- retry-budget enforcement for timed-out or abandoned tasks
- reconciliation handling for `side_effect_state = unknown`
- continued composition with approval timeout processing and runnable-workflow draining already present in the tick loop

---

## 8. `workflow_wait` Migration Semantics

### 8.1 Current state

`workflow_wait` is the primary orchestration primitive for parent/child coordination.

### 8.2 Target state

`workflow_wait` becomes:

- an inspection/debugging primitive
- an explicit compatibility shim
- optional for new agents

### 8.3 Migration behavior

If a parent already issued `workflow_wait`:

- native wake-up resolves the wait
- the parent sees the same child result, but wake-up came from a state transition rather than polling

If a parent did not issue `workflow_wait`:

- the next turn receives `ChildStateNotification` through scheduler signal delivery and turn-start context injection
- prompt logic may continue using `workflow_state` for inspection, but correct orchestration no longer depends on active polling

### 8.4 No-op semantics

Post-migration, calling `workflow_wait` after the child is already resolved is valid and should return immediately with the same typed state.

---

## 9. Single-Flight Semantics

### 9.1 Initial scope

Apply single-flight only to durable operations:

- install
- promote
- rollback
- artifact-backed durable build/promote boundaries

### 9.2 Dedupe key

Default key:

```text
(workflow_id, stage_kind, agent_id, artifact_ref)
```

Reasoning-only installs use normalized intent digest instead of `artifact_ref`.

### 9.3 Coalesced response shape

```json
{
  "ok": true,
  "status": "coalesced",
  "existing_task_id": "task-123",
  "retry_advice": "wait",
  "dedupe_key": "install:wf-123:my-agent:ar.abc"
}
```

Coalescing must be explicit, not silent.

### 9.4 Ordering with gate creation

Single-flight check must run **before** approval/gate creation for durable operations.

Reason:

- if the first install/promote task is already pending approval, a duplicate request must coalesce to that existing task
- if single-flight runs after gate creation, duplicate approval requests will still be minted

Ordering for durable operations:

1. compute dedupe key
2. check for active equivalent operation
3. if duplicate exists, return `status: coalesced`
4. otherwise proceed to `GateService` / approval path if needed

### 9.5 Side-effect interaction

- `side_effect_state = none` (`SideEffectState::NoSideEffect`) -> safe retry or coalescing path depends on retry budget
- `side_effect_state = committed` -> do not retry blindly
- `side_effect_state = unknown` -> stop and require reconciliation semantics

---

## 10. Stage-Local Retry Policy

### 10.1 Policy declaration

Retry is opt-in.

Possible declaration forms:

- manifest metadata for agent-owned stage policy
- workflow/task spawn metadata for per-task override

Example:

```json
{
  "retry_policy": {
    "transient_infra": { "max_retries": 1 },
    "timeout": { "max_retries": 1 },
    "install_conflict": { "max_retries": 0 }
  }
}
```

If absent, gateway emits typed failure and returns control to the parent.

### 10.2 Normalization algorithm

Pseudo-logic:

```text
classify failure -> load task retry policy -> compare retry_count to budget
-> compute retry_advice -> persist retry_count/last_failure_class
-> emit event if exhausted
```

### 10.3 Budget exhaustion

On exhaustion:

- mark task `Failed`
- set `retry_advice = DoNotRetry`
- emit `workflow.stage_budget_exhausted`
- retain legacy prose hint during compatibility window

---

## 11. Constitutional Mapping

This RFC assumes the constitutional changes proposed in the design plan and implements them in this order:

1. R+19 mechanical failure classification
2. Ri-0.14 parent wake-up plus `WaitingForChild`
3. R+20 single-flight
4. R+22 side-effect state tracking
5. R+21 stage-local retry budget
6. R-5.11 envelope extension and prompt simplification

Implementation must be gated by constitutional acceptance where the relevant rule is not already ratified.

---

## 12. Compatibility Strategy

### 12.1 Dual emission window

For one full release cycle, emit both:

- typed fields: `failure_class`, `retry_advice`, `side_effect_state`, `dedupe_key`
- legacy fields: `error_type`, `message`, `repair_hint`

### 12.2 Prompt migration

Migration expectation:

- old prompts continue reading prose fields
- new prompts prefer typed fields
- no prompt should be required to interpret both for correctness

### 12.3 External consumers

Legacy prose fields should be deprecated after the migration window, not removed from response shapes immediately.

---

## 13. Test Plan

### 13.1 Unit tests

Add focused tests for:

- failure classification mapping
- retry advice normalization
- side-effect state mapping
- dedupe-key construction
- single-flight coalescing

### 13.2 Integration tests

Add end-to-end tests for:

1. parent wake-up without polling
2. `workflow_wait` compatibility during migration
3. install conflict returns `install_conflict + do_not_retry`
4. transient infra retries exactly once when policy allows
5. budget exhaustion emits event and stops retries
6. duplicate install requests coalesce to existing task

### 13.3 Constitutional tests

Add new tests mirroring proposed rule IDs:

- `constitution_r_5_14_mechanical_failure_classification.rs`
- `constitution_r_6_24_single_flight_protection.rs`
- `constitution_r_6_25_stage_local_retry_budget.rs`
- `constitution_r_6_26_side_effect_state.rs`
- `constitution_right_ri_0_14.rs`

### 13.4 Determinism tests

All classification and retry normalization must be pure functions of persisted state and structured tool/runtime observations.

No LLM calls.
No network fetches.
No hidden time-based branches except explicit timeout state.

---

## 14. Rollout Plan

### Slice A: Typed classification, no behavior change

Implement:

- enums and typed envelope fields
- failure classification layer
- dual emission

Do not implement:

- auto wake-up
- single-flight
- retry execution

Success criterion:

- typed fields are present and stable in workflow-relevant failures

### Slice B: Parent wake-up and `workflow_wait` compatibility

Implement:

- `WaitingForChild`
- parent wake-up on child transitions
- compatibility satisfaction for `workflow_wait`

Success criterion:

- new orchestration flows work without polling
- old flows still work with polling

### Slice C: Single-flight + side-effect state

Implement:

- dedupe key generation
- coalesced responses
- side-effect state reporting on durable operations

Success criterion:

- duplicate install/promote flows no longer fan out into parallel durable work

### Slice D: Declared retry budget

Implement:

- task-row retry metadata
- workflow-layer retry normalization
- budget exhaustion eventing

Success criterion:

- declared transient-infra retries are bounded and observable

### Slice E: Prompt cleanup

Update planner/factory/builder prompts to rely on typed workflow semantics instead of prose string matching.

Success criterion:

- prompt boilerplate is materially reduced without behavioral regressions

---

## 15. Risks and Mitigations

### Risk 1: Gateway becomes semantic

Mitigation:

- keep classification sources limited to gateway-observed facts
- keep retry opt-in and bounded
- do not let gateway choose product-level next steps

### Risk 2: Compatibility break for old prompts

Mitigation:

- dual emission for one release cycle
- `workflow_wait` remains functional
- typed wake-up is additive before being relied upon

### Risk 3: Dedupe key too coarse or too fine

Mitigation:

- start with install/promote/rollback only
- emit dedupe key in responses and events for observability
- add targeted regression tests before widening scope

### Risk 4: Retry policy conflicts with constitutional dumbness

Mitigation:

- require explicit policy declaration
- keep retry stage-local and mechanically identical only
- treat undeclared retry as parent semantic responsibility

---

## 16. Exit Criteria

This RFC is complete when all of the following are true:

1. workflow-relevant failures emit typed mechanical classification
2. parent wake-up no longer requires prompt polling for correctness
3. duplicate durable operations are coalesced mechanically
4. stage-local retry is bounded, declared, and observable
5. planner/factory/builder prompts no longer rely on string-matching runtime errors for normal orchestration

---

## 17. Recommendation

Start with Slice A and Slice B.

They deliver the highest reduction in prompt bloat and orchestration fragility while keeping the gateway within the dumbness boundary.

The guiding implementation rule is:

> The gateway may observe, classify, dedupe, suspend, wake, and bound retries mechanically. Agents remain responsible for deciding what those facts mean for the user's goal.
