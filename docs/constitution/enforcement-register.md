# Enforcement Register (generated)

> **Generated** from `autonoetic-gateway/src/enforcement_register.rs`. Do not edit by hand — run the register generator. Maps each constitutional **clause** — a principle (binds the agent) or a right (binds the gateway) — to the mechanical checks, code, tests, and config that enforce it. Legacy `R-x.y` / `Ri-x.y` IDs are preserved as stable reference keys. See `docs/design/constitution-restructure.md`.

## Bind-direction summary

1 principle(s) (bind the agent), 2 right(s) (bind the gateway), 2 obligation(s) (bind the decider). Counts are partial while migration (#303) is in progress — not the design ratio.

## Principles (bind: agent)

### P-7 — Bounded progress

A session is halted when it stops making progress, on a closed, configurable set of mechanically-detected non-progress conditions, each emitting a typed, attributable reason. No condition relies on agent self-report.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `P-7.5` | `tool_failure_budget` | `guard.rs::register_failure + check_loop` | `runtime::guard::tests::test_loop_guard_trips_on_tool_failure_budget` | `loop_guard.max_tool_failures` |
| `P-7.7` | `no_meaningful_progress` | `guard.rs::check_loop` | `runtime::guard::tests::test_loop_guard_trips_on_max_loops` | `loop_guard.max_loops_without_progress` |
| `P-7.19` | `rotating_polling_pattern` | `guard.rs::register_progress_inner (window + trip) + check_loop` | `runtime::guard::tests::rotating_polling_pattern_with_five_tools_trips` | `loop_guard.rotation_window_size, loop_guard.rotation_distinct_floor` |
| `P-7.20` | `child_failure_budget` | `guard.rs::register_child_failure + check_loop` | `runtime::guard::tests::test_loop_guard_trips_on_child_failures` | `loop_guard.max_child_failures` |

## Rights (bind: gateway)

### Ri-0.13 — Reasoning privacy

An agent's internal reasoning is private-under-law: not used by the gateway as a basis for policy decisions, recorded to the agent's own causal chain for forensic review, and disclosed to other parties only through capability-gated audit.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `Ri-0.13` | `reasoning_disclosure_capability_gated` | `runtime/tools/observability.rs (reasoning audit) + disclosure gating` | `constitution_private_reasoning_c.rs::ri_0_13c_execute_reads_and_discloses` | — |

### Ri-0.14 — Wake-up over polling

When a child task reaches a terminal state or resolves a gate, the gateway wakes the parent with typed child state. Parents are not required to poll to discover child-state transitions.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `Ri-0.14` | `child_state_wakeup` | `scheduler/workflow_store.rs::update_task_run_status (send_child_state_notification) + scheduler/signal.rs + scheduler/task_notify.rs` | `constitution_right_ri_0_14.rs::child_waiting_transition_emits_typed_parent_wakeup_event` | `default_workflow_wait_secs` |

## Obligations (bind: decider)

### O-1 — Motivated decision

A decision owes a motivation, graduated by stakes. A rejection/abort, or an approval of an elevated-authority or external/irreversible action, is BLOCKING: it does not commit until a non-empty reason is recorded. Silent rejection by a decider is as illegitimate as a gateway denial with no rule ID (Ri-0.3).

| rule id | check | code | test | config |
|---|---|---|---|---|
| `O-1` | `decider_obligation_motivation` | `scheduler/approval.rs::enforce_decider_motivation (classifier decision_is_blocking) at the decide_request_with_options chokepoint; emits decider_obligation.refused/.satisfied` | `constitution_o_1_decider_motivation.rs + scheduler::approval::tests::decider_obligation_emits_tagged_o1_event` | `decider_obligations.enabled` |

### O-2 — Attributed decision

Every decision is attributed to the deciding principal (id + kind) on the causal chain and cannot be reattributed. The agent under decision can always tell who decided and what kind of principal they are.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `O-2` | `decider_attribution` | `decided_by + decided_by_kind on the approval (principal::decider_principal_kind, #361) + actor bound into the causal-chain entry hash (causal_chain.rs)` | `constitution_o_1_decider_motivation.rs` | — |

