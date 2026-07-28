//! Egress domain integration tests, grouped into one binary (#922).
//!
//! RFC data-envelopes-egress-localization: label resolution at the tool-result
//! boundary, the LLM chokepoint, compression eligibility, bundle floor +
//! argument taint, and taint-following routing. Grouping these suites into a
//! single test binary replaces five separate compile+link units with one
//! (all are tempfile-isolated and share no external state — no ports, no
//! fixed paths — so cohabitation in one process is safe under both
//! `cargo test` and nextest).

mod chokepoint_canary;
mod compression_eligibility;
mod floor_and_taint;
mod routing;
mod source_rules;
