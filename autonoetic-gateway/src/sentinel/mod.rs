//! Security sentinel — deterministic check engine (Phase 1).
//!
//! This module provides the gateway-side, LLM-free scan that runs
//! credential-pattern matching, capability-accretion detection,
//! approval-bypass detection, and sandbox-escape pattern matching.
//!
//! The checks produce `SecurityFinding` records that are persisted in the
//! `security_findings` table. Each finding also emits a
//! `security_finding_recorded` event to the causal chain of the sentinel
//! session (when a session context is provided).
//!
//! # Separation of powers
//!
//! The sentinel *reads* from state; it does not mutate anything except the
//! append-only `security_findings` table and the causal chain.

pub mod checks;
pub mod runner;

pub use runner::{SentinelRunner, SweepResult};
