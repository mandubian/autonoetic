//! Security sentinel — deterministic and heuristic check engine.
//!
//! ## Phase 1 (deterministic)
//!
//! Pure regex and SQL checks that run without any LLM calls. Findings may
//! reach `critical` severity because they are reproducible by anyone who
//! re-runs the same query.
//!
//! - Credential-pattern regex over causal-event payloads
//! - Capability-accretion detection via SQL over `promotion_history`
//! - Approval-bypass pattern detection
//! - Sandbox-escape recorded-attempt table scan + escape pattern regex
//!
//! ## Phase 2 (LLM-judgment heuristics)
//!
//! Structural pattern matching that requires human or LLM reasoning to confirm.
//! Findings land at `warning` severity with `llm_judgment` reproducibility.
//!
//! - Prompt-injection surface detection on SKILL.md instruction bodies
//!   (reads files from `agents_dir` when set on the runner)
//! - Session-cluster anomaly detection: rapid failure bursts, repeated
//!   identical sandbox_exec calls
//!
//! Curator decision-journal audits (Phase 2, issue #30 dependency) are
//! deferred until the memory curator decision journal lands.
//!
//! ## Phase 5 (planned)
//!
//! Emission of `security_finding_recorded` events to the causal chain will be
//! added in Phase 5 (scheduling integration), when the sentinel runs with a
//! dedicated session context. The current runner persists findings to the
//! `security_findings` table only — no causal event is emitted here yet.
//!
//! # Separation of powers
//!
//! The sentinel *reads* from state; it does not mutate anything except the
//! append-only `security_findings` table.

pub mod checks;
pub mod dual_sweep;
pub mod runner;

pub use dual_sweep::{DualSweepResult, DualSweepRunner};
pub use runner::{SentinelRunner, SweepResult};
