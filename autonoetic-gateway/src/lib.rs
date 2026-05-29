//! Autonoetic Gateway — core daemon crate.
//!
//! Re-exports shared types from `autonoetic_types` and provides
//! Gateway-specific logic: config loading, agent scanning, runtime
//! lock resolution, and sandbox management.

pub mod agent;
pub mod artifact_store;
pub mod bootstrap;
pub mod capsule;
pub mod causal_chain;
pub mod config;
pub mod constitution_digest;
pub mod constitution_glossary;
pub mod enforcement_register;
pub mod execution;
pub mod fail_mode;
pub mod interaction_answer;
pub mod layer_store;
pub mod llm;
pub mod log_redaction;
pub mod policy;
pub mod post_promotion_review;
pub mod router;
pub mod runtime;
pub mod runtime_lock;
pub mod sandbox;
pub mod scheduler;
pub mod sentinel;
pub mod server;
pub mod tracing;
pub mod vault;

pub use agent::{cached, scan_agents, AgentRepository, LoadedAgent};
pub use artifact_store::ArtifactStore;
pub use autonoetic_types::agent::AgentMeta;
pub use autonoetic_types::config::GatewayConfig;
pub use autonoetic_types::layer::{ArtifactLayer, CapturedLayer, LayerManifest};
pub use autonoetic_types::runtime_lock::RuntimeLock;
pub use bootstrap::{bootstrap_agents, ensure_vault_key_for_bootstrap_workspace};
pub use causal_chain::CausalLogger;
pub use execution::{GatewayExecutionService, SpawnResult};
pub use interaction_answer::{
    InteractionAnswerOutcome, InteractionAnswerParams, InteractionResolveAndAnswerParams,
};
pub use layer_store::LayerStore;
pub use llm::{build_driver, LlmDriver};
pub use policy::PolicyEngine;
pub use router::{JsonRpcRequest, JsonRpcResponse, JsonRpcRouter};
pub use runtime::openrouter_catalog::OpenRouterCatalog;
pub use runtime::session_budget::SessionBudgetRegistry;
pub use runtime::tools::resolve_target_to_agent_ref;
pub use runtime_lock::resolve_runtime_lock;
pub use sandbox::SandboxRunner;
pub use scheduler::system_agents::reconcile_system_agents;
pub use sentinel::{ensure_sentinel_scheduled_jobs, run_due_sentinel_jobs};
pub use server::GatewayServer;
pub use tracing::session_tracer::{EventScope, EventSeq, SessionId, TraceSession};
pub use vault::Vault;
