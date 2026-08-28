//! Agent domain integration tests, grouped into one binary (#922).
//!
//! Formerly one compile+link unit per file; see AGENTS.md "Testing".
//! External-state audit: no ports, no fixed paths, no singletons
//! (safe to cohabit one process under cargo test and nextest).

#[path = "../support/mod.rs"]
mod support;

mod adapter_scripts;
mod adapter_staleness;
mod adapter_wrapper;
mod executor_helpers;
mod remote_access_any_preapproval;
mod inspect;
mod install_approval_e2e;
mod install_smoke_test_gate;
mod message_midturn_delivery;
mod messaging;
mod singleton_dedup;
mod suspend_rpc;
