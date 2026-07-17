//! Trajectory-monitor feedback types for divergence classification.

use serde::{Deserialize, Serialize};

use crate::tool_error::ToolErrorType;

/// A unit of feedback the gateway gave to the agent that the agent is expected
/// to incorporate on the next turn.
///
/// Two shapes:
/// - `Validation { rule, field_path }`: a response-validation violation the
///   gateway repaired against.
/// - `ToolError { tool, error_type, message_signature }`: a typed tool failure
///   returned to the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FeedbackEvent {
    Validation {
        /// The validation rule that fired (e.g. `output_schema`,
        /// `delegated_without_spawn`).
        rule: String,
        /// Normalized field path within the reply, when available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        field_path: Option<String>,
    },
    ToolError {
        /// Tool name that failed.
        tool: String,
        /// Typed error classification.
        error_type: ToolErrorType,
        /// Normalized message text so semantically-identical errors compare
        /// equal even when the raw message varies (e.g. timestamps, ids).
        message_signature: String,
    },
}

impl FeedbackEvent {
    /// Stable string key for identity comparisons across turns.
    pub fn signature_key(&self) -> String {
        match self {
            Self::Validation { rule, field_path } => {
                let path = field_path.as_deref().unwrap_or("*");
                format!("validation:{rule}:{path}")
            }
            Self::ToolError {
                tool,
                error_type,
                message_signature,
            } => format!("tool_error:{tool}:{error_type}:{message_signature}"),
        }
    }

    /// Short human-readable category for causal-event payloads.
    pub fn category(&self) -> &'static str {
        match self {
            Self::Validation { .. } => "validation",
            Self::ToolError { .. } => "tool_error",
        }
    }
}
