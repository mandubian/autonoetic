pub mod agent;
pub mod capsule;
pub mod chat;
pub mod common;
/// Guard: every clap subcommand appears in `docs/reference/cli.md`. Test-only.
#[cfg(test)]
mod docs_coverage;
pub mod eval;
pub mod gateway;
pub mod mcp;
pub mod model_discovery;
pub mod recording;
pub mod room;
pub mod rpc;
pub mod run;
pub mod security;
pub mod sentinel_experiment;
pub mod improve;
pub mod session;
pub mod terminal;
pub mod trace;
pub mod watchdog;
