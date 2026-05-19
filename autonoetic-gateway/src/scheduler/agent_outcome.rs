//! Re-export task completion parsing from shared types (gateway writes `agent_outcome` on events).

pub use autonoetic_types::task_completion::{
    extract_agent_outcome, AgentOutcome, TaskCompletionPresentation,
};
