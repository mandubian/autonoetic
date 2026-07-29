//! Guard/budget domain integration tests, grouped into one binary (#922).

#[path = "../support/mod.rs"]
mod support;

mod host_probe_budget;
mod root_budget_circuit_breaker;
mod spawn_identity_loop_guard;
mod tool_guard_regressions;
mod web_host_probe_budget;
