//! Bootstrap domain integration tests, grouped into one binary (#922).
//!
//! Gateway startup stack budget. External-state audit: no ports, no
//! fixed paths, no singletons (safe to cohabit one process under cargo
//! test and nextest).

#[path = "../support/mod.rs"]
mod support;

mod gateway_bootstrap_stack_budget;
