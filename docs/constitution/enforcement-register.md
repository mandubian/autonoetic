# Enforcement Register (generated)

> **Generated** from `autonoetic-gateway/src/enforcement_register.rs`. Do not edit by hand — run the register generator. Maps each constitutional **principle** to the mechanical checks, code, tests, and config that enforce it. Legacy `R-x.y` IDs are preserved as stable reference keys. See `docs/design/constitution-restructure.md`.

## P-7 — Bounded progress

A session is halted when it stops making progress, on a closed, configurable set of mechanically-detected non-progress conditions, each emitting a typed, attributable reason. No condition relies on agent self-report.

| legacy id | check | code | test | config |
|---|---|---|---|---|
| `R-7.5` | `tool_failure_budget` | `guard.rs::register_failure + check_loop` | `runtime::guard::tests::test_loop_guard_trips_on_tool_failure_budget` | `loop_guard.max_tool_failures` |
| `R-7.7` | `no_meaningful_progress` | `guard.rs::check_loop` | `runtime::guard::tests::test_loop_guard_trips_on_max_loops` | `loop_guard.max_loops_without_progress` |
| `R-7.19` | `rotating_polling_pattern` | `guard.rs::register_progress_inner (window + trip) + check_loop` | `runtime::guard::tests::rotating_polling_pattern_with_five_tools_trips` | `loop_guard.rotation_window_size, loop_guard.rotation_distinct_floor` |
| `R-7.20` | `child_failure_budget` | `guard.rs::register_child_failure + check_loop` | `runtime::guard::tests::test_loop_guard_trips_on_child_failures` | `loop_guard.max_child_failures` |

