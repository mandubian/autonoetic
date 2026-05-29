# Enforcement Register (generated)

> **Generated** from `autonoetic-gateway/src/enforcement_register.rs`. Do not edit by hand — run the register generator. Maps each constitutional **clause** — a principle (binds the agent) or a right (binds the gateway) — to the mechanical checks, code, tests, and config that enforce it. Legacy `R-x.y` / `Ri-x.y` IDs are preserved as stable reference keys. See `docs/design/constitution-restructure.md`.

## Bind-direction summary

1 principle(s) (bind the agent), 2 right(s) (bind the gateway). Counts are partial while migration (#303) is in progress — not the design ratio.

## Principles (bind: agent)

### P-7 — Bounded progress

A session is halted when it stops making progress, on a closed, configurable set of mechanically-detected non-progress conditions, each emitting a typed, attributable reason. No condition relies on agent self-report.

| legacy id | check | code | test | config |
|---|---|---|---|---|
| `R-7.5` | `tool_failure_budget` | `guard.rs::register_failure + check_loop` | `runtime::guard::tests::test_loop_guard_trips_on_tool_failure_budget` | `loop_guard.max_tool_failures` |
| `R-7.7` | `no_meaningful_progress` | `guard.rs::check_loop` | `runtime::guard::tests::test_loop_guard_trips_on_max_loops` | `loop_guard.max_loops_without_progress` |
| `R-7.19` | `rotating_polling_pattern` | `guard.rs::register_progress_inner (window + trip) + check_loop` | `runtime::guard::tests::rotating_polling_pattern_with_five_tools_trips` | `loop_guard.rotation_window_size, loop_guard.rotation_distinct_floor` |
| `R-7.20` | `child_failure_budget` | `guard.rs::register_child_failure + check_loop` | `runtime::guard::tests::test_loop_guard_trips_on_child_failures` | `loop_guard.max_child_failures` |

## Rights (bind: gateway)

### Ri-0.13 — Reasoning privacy

An agent's internal reasoning is private-under-law: not used by the gateway as a basis for policy decisions, recorded to the agent's own causal chain for forensic review, and disclosed to other parties only through capability-gated audit.

| legacy id | check | code | test | config |
|---|---|---|---|---|
| `Ri-0.13` | `reasoning_disclosure_capability_gated` | `runtime/tools/observability.rs (reasoning audit) + disclosure gating` | `constitution_private_reasoning_c.rs::ri_0_13c_execute_reads_and_discloses` | — |

### Ri-0.14 — Wake-up over polling

When a child task reaches a terminal state or resolves a gate, the gateway wakes the parent with typed child state. Parents are not required to poll to discover child-state transitions.

| legacy id | check | code | test | config |
|---|---|---|---|---|
| `Ri-0.14` | `child_state_wakeup` | `scheduler/workflow_store.rs::update_task_run_status (send_child_state_notification) + scheduler/signal.rs + scheduler/task_notify.rs` | `constitution_right_ri_0_14.rs::child_waiting_transition_emits_typed_parent_wakeup_event` | `default_workflow_wait_secs` |

