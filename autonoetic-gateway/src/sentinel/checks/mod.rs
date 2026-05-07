//! Security sentinel check modules.
//!
//! - Phase 1 (deterministic): `credential`, `capability_accretion`,
//!   `approval_bypass`, `sandbox_escape`, `supply_chain`
//! - Phase 2 (LLM-judgment heuristics): `prompt_injection`, `session_cluster`

pub mod approval_bypass;
pub mod capability_accretion;
pub mod credential;
pub mod prompt_injection;
pub mod sandbox_escape;
pub mod session_cluster;
pub mod supply_chain;
