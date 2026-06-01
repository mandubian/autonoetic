//! Shared data models for the Autonoetic Agent System.
//!
//! Types defined here are the canonical Rust representations of the schemas
//! documented in `data_models.md`.

pub mod agent;
pub mod agent_revision;
pub mod artifact;
pub mod background;
pub mod capability;
pub mod capsule;
pub mod causal_chain;
pub mod config;
pub mod disclosure;
pub mod escalation;
pub mod evaluation;
pub mod hooks;
pub mod id_format;
pub mod improvement_cycle;
pub mod layer;
pub mod memory;
pub mod notification;
pub mod operator_activity;
pub mod plan_frame;
pub mod promotion;
pub mod recording;
pub mod redaction;
pub mod runtime_lock;
pub mod scheduled_job;
pub mod schema_enforcement;
pub mod semantic_diff;
pub mod security;
pub mod session_outcome;
pub mod task_board;
pub mod task_completion;
pub mod tool_error;
pub mod workbench;
pub mod workflow;
