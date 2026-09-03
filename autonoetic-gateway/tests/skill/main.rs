//! Skill domain integration tests, grouped into one binary (#922).
//!
//! Single-door activation and import provenance (P-9.15/P-9.16).
//! External-state audit: no ports, no fixed paths, no singletons
//! (safe to cohabit one process under cargo test and nextest).

#[path = "../support/mod.rs"]
mod support;

mod skill_install_one_door_provenance;
