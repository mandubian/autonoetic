//! Workflow domain integration tests, grouped into one binary (#922).
//!
//! Formerly one compile+link unit per file; see AGENTS.md "Testing".
//! External-state audit: no ports, no fixed paths, no singletons
//! (safe to cohabit one process under cargo test and nextest).

#[path = "../support/mod.rs"]
mod support;

mod approval_resume;
mod approval_spawn_gate;
mod completion_guard;
mod parent_child_wait_suspension;
mod state_promotion_verdict_source;
mod wait_signal_driven;
