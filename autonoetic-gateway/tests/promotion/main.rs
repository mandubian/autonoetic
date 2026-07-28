//! Promotion domain integration tests, grouped into one binary (#922).
//!
//! Formerly one compile+link unit per file; see AGENTS.md "Testing".
//! External-state audit: no ports, no fixed paths, no singletons
//! (safe to cohabit one process under cargo test and nextest).

#[path = "../support/mod.rs"]
mod support;

mod attempt_exhaustion;
mod gate_hardening;
mod gate_mocked_network_e2e;
mod gate_network_isolation_decision;
mod governor;
mod record_e2e;
mod record_evaluator_fail;
mod record_findings_validation;
mod record_reject;
mod trace_evidence;
