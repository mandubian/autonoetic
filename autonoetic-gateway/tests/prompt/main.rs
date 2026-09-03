//! Prompt domain integration tests, grouped into one binary (#922).
//!
//! Per-agent prompt composition budget (turn-1 / working / steady-state
//! ceilings; prints a per-layer breakdown). External-state audit: no
//! ports, no fixed paths, no singletons (safe to cohabit one process
//! under cargo test and nextest).

mod prompt_composition_budget;
