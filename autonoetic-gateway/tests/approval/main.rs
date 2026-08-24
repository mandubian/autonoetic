//! Approval domain integration tests, grouped into one binary (#922).

#[path = "../support/mod.rs"]
mod support;

mod approve_resume_detached;
mod grant_revocation;
mod host_constant_resolution;
mod interaction_rpc_surface;
mod scope_targets;
mod waiter_fanin;
