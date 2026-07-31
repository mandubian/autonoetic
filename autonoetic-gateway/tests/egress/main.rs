//! Egress domain integration tests, grouped into one binary (#922).
//!
//! RFC data-envelopes-egress-localization: label resolution at the tool-result
//! boundary, the LLM chokepoint, compression eligibility + per-label-band
//! compression (§5.7), bundle floor + argument taint, and taint-following
//! routing. Grouping these suites into a single test binary replaces five
//! separate compile+link units with one (all are tempfile-isolated and share
//! no external state — no ports, no fixed paths — so cohabitation in one
//! process is safe under both `cargo test` and nextest).

mod artifact_labels;
mod child_taint_propagation;
mod chokepoint_canary;
mod compartment;
mod compression_eligibility;
mod floor_and_taint;
mod label_listing_rpc;
mod mixed_session_e2e;
mod operator_message_label;
mod phase4_boundaries;
mod phase4_capsule;
mod phase4_declassification;
mod phase4_mcp;
mod phase4_ofp;
mod phase4_sandbox;
mod phase4_web_hooks;
mod proposal_authoring;
mod routing;
mod shared_env;
mod source_rules;
mod stored_content;
