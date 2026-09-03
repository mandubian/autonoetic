//! Sandbox domain integration tests, grouped into one binary (#922).
//!
//! In-sandbox network grants under real bubblewrap (manual `#[ignore]`d
//! e2e, bwrap-gated). External-state audit: no ports, no fixed paths,
//! no singletons (safe to cohabit one process under cargo test and
//! nextest).

mod sandbox_network_grant_bwrap_e2e;
