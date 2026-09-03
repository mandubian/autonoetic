//! Anomaly domain integration tests, grouped into one binary (#922).
//!
//! Ri-0.18 capability-free intake + the per-reporter flood cap, and the
//! adjudication tool (terminal decisions, SLA). External-state audit:
//! no ports, no fixed paths, no singletons (safe to cohabit one process
//! under cargo test and nextest).

#[path = "../support/mod.rs"]
mod support;

mod anomaly_adjudicate_tool_integration;
mod anomaly_flag_integration;
