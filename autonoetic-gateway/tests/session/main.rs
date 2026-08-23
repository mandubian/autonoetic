//! Session domain integration tests, grouped into one binary (#922).
//!
//! Formerly one compile+link unit per file; see AGENTS.md "Testing".
//! External-state audit: no ports, no fixed paths, no singletons
//! (safe to cohabit one process under cargo test and nextest).

#[path = "../support/mod.rs"]
mod support;

mod constitution_pin_drift_notice;
mod envelope_discovery;
mod envelope_locking;
mod envelope_promote_with;
mod escalate;
mod export;
mod fork;
mod handoff;
mod grant_close_preservation;
mod inference;
mod outcome_rpc;
mod pause_cooperative;
mod stream_fallback;
mod report;
mod residency;
mod trace;
mod timeline_jsonrpc;
