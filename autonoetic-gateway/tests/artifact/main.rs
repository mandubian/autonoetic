//! Artifact domain integration tests, grouped into one binary (#922).

#[path = "../support/mod.rs"]
mod support;

mod build_ref;
mod exec_lifecycle;
mod fork_lineage;
mod ref_global_on_promote;
