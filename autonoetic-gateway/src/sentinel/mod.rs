//! Security sentinel — deterministic check engine (Phase 1).
//!
//! This module provides the gateway-side, LLM-free scan that runs
//! credential-pattern matching, capability-accretion detection,
//! approval-bypass detection, and sandbox-escape pattern matching.
//!
//! The checks produce `SecurityFinding` records that are persisted in the
//! `security_findings` table.
//!
//! NOTE: Emission of `security_finding_recorded` events to the causal chain is
//! planned for Phase 5 (scheduling integration), when the sentinel runs with a
//! dedicated session context. The current runner persists findings to the
//! `security_findings` table only — no causal event is emitted here yet.
//!
//! # Separation of powers
//!
//! The sentinel *reads* from state; it does not mutate anything except the
//! append-only `security_findings` table.

pub mod checks;
pub mod runner;

pub use runner::{SentinelRunner, SweepResult};
