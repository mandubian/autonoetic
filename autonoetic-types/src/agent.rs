//! Agent Manifest types — the Rust representation of `SKILL.md` frontmatter.

use crate::background::BackgroundPolicy;
use crate::disclosure::DisclosurePolicy;
use serde::{Deserialize, Serialize};

use crate::capability::Capability;

/// Runtime declaration block from the SKILL.md frontmatter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeDeclaration {
    pub engine: String,
    pub gateway_version: String,
    pub sdk_version: String,
    #[serde(rename = "type")]
    pub runtime_type: String, // "stateful" | "stateless"
    pub sandbox: String, // "bubblewrap" | "docker" | "microvm" | "wasm"
    pub runtime_lock: String,
}

/// Core agent identity fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentIdentity {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// LLM configuration for the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub temperature: f64,
    pub fallback_provider: Option<String>,
    pub fallback_model: Option<String>,
    /// Set to true if the provider only supports basic chat (no tools at all)
    /// (e.g., Z.AI GLM models via OpenRouter)
    #[serde(default)]
    pub chat_only: bool,
    /// Optional context window size (tokens) for UX such as "% of context used" in the CLI.
    /// If unset, use env `AUTONOETIC_LLM_CONTEXT_WINDOW` or omit percentage.
    #[serde(default)]
    pub context_window_tokens: Option<u32>,
}

/// One provider round-trip: token counts and optional context window utilization.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct LlmExchangeUsage {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Declared context window used for `input_context_pct` (echo for clients).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<u32>,
    /// Prompt (`input_tokens`) as a percentage of `context_window_tokens` when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_context_pct: Option<f32>,
    /// Estimated USD for this completion (OpenRouter catalog pricing × token counts) when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
}

/// Resource limits enforced by the Gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub max_memory_mb: u64,
    pub max_execution_time_sec: u64,
    pub token_budget_monthly: Option<u64>,
}

/// The full parsed Agent Manifest (SKILL.md frontmatter).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentManifest {
    pub version: String,
    pub runtime: RuntimeDeclaration,
    pub agent: AgentIdentity,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    pub llm_config: Option<LlmConfig>,
    pub limits: Option<ResourceLimits>,
    #[serde(default)]
    pub background: Option<BackgroundPolicy>,
    #[serde(default)]
    pub disclosure: Option<DisclosurePolicy>,
    #[serde(default)]
    pub io: Option<AgentIO>,
    #[serde(default)]
    pub middleware: Option<Middleware>,
    /// Response contract declared in the agent's own SKILL.md frontmatter.
    /// When present, the gateway uses this as the default contract for any
    /// spawn of this agent (unless the caller supplies an override in spawn metadata).
    #[serde(default)]
    pub response_contract: Option<serde_json::Value>,
    /// Execution mode: Script (fast path, no LLM) or Reasoning (default, LLM-driven).
    #[serde(default)]
    pub execution_mode: ExecutionMode,
    /// Entry script for Script mode. Relative path from agent directory.
    #[serde(default)]
    pub script_entry: Option<String>,
    /// Remote gateway URL for distributed agents. When set, SDK uses HTTP mode.
    #[serde(default)]
    pub gateway_url: Option<String>,
    /// Authentication token for remote gateway (Bearer token).
    #[serde(default)]
    pub gateway_token: Option<String>,
}

/// Middleware hooks declared in the agent's own manifest (replaces overlay-based hooks).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Middleware {
    /// Script/command to run on user input before passing to the LLM.
    #[serde(default)]
    pub pre_process: Option<String>,
    /// Script/command to run on LLM output before returning to the user.
    #[serde(default)]
    pub post_process: Option<String>,
}

/// Execution mode for an agent: script-only or LLM-driven reasoning.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// Agent runs a script directly in sandbox, bypassing LLM entirely.
    Script,
    /// Default: full LLM-driven reasoning loop.
    #[default]
    Reasoning,
}

/// I/O schema contract for an agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentIO {
    /// JSON Schema describing accepted input.
    #[serde(default)]
    pub accepts: Option<serde_json::Value>,
    /// JSON Schema describing produced output.
    #[serde(default)]
    pub returns: Option<serde_json::Value>,
}

/// Response contract declared in agent metadata for post-execution validation.
///
/// When present, the gateway validates the agent's SpawnResult against these
/// constraints before returning to the caller. Violations trigger a ToolError
/// with a repair hint; the agent may retry within bounded loop/duration limits.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResponseContract {
    /// Artifact names the agent must produce (e.g. "main_report.md").
    #[serde(default)]
    pub required_artifacts: Vec<String>,

    /// Maximum number of artifacts allowed. Default: no limit.
    #[serde(default)]
    pub max_artifacts: Option<usize>,

    /// Maximum total artifact size in megabytes. Default: no limit.
    #[serde(default)]
    pub max_total_size_mb: Option<u64>,

    /// Maximum reply length in characters. Default: no limit.
    #[serde(default)]
    pub max_reply_length_chars: Option<usize>,

    /// Optional JSON Schema the reply text must conform to (when the reply is JSON).
    #[serde(default)]
    pub output_schema: Option<serde_json::Value>,

    /// Regex patterns that must NOT appear in the reply text.
    /// Used for safety scanning (secret leaks, forbidden paths, etc.).
    #[serde(default)]
    pub prohibited_text_patterns: Vec<String>,

    /// Minimum number of successful `artifact.build` tool invocations required
    /// in this session branch. This is durable evidence from execution traces,
    /// not inferred from reply text.
    #[serde(default)]
    pub min_artifact_builds: Option<u32>,

    /// Max validation retry loops (1–8). Default: 1.
    #[serde(default = "default_validation_max_loops")]
    pub validation_max_loops: u32,

    /// Max wall-clock duration for validation retries in milliseconds (0–30000). Default: 500.
    #[serde(default = "default_validation_max_duration_ms")]
    pub validation_max_duration_ms: u64,
}

fn default_validation_max_loops() -> u32 {
    1
}

fn default_validation_max_duration_ms() -> u64 {
    500
}

impl ResponseContract {
    /// Clamp loop/duration bounds to allowed ranges.
    pub fn normalize(&mut self) {
        self.validation_max_loops = self.validation_max_loops.clamp(1, 8);
        self.validation_max_duration_ms = self.validation_max_duration_ms.clamp(0, 30_000);
        if let Some(n) = self.max_artifacts {
            self.max_artifacts = Some(n.clamp(1, 100));
        }
        if let Some(n) = self.min_artifact_builds {
            self.min_artifact_builds = Some(n.clamp(0, 32));
        }
    }

    /// Returns true if no validation rules are declared.
    pub fn is_empty(&self) -> bool {
        self.required_artifacts.is_empty()
            && self.max_artifacts.is_none()
            && self.max_total_size_mb.is_none()
            && self.max_reply_length_chars.is_none()
            && self.output_schema.is_none()
            && self.prohibited_text_patterns.is_empty()
            && self.min_artifact_builds.is_none()
    }
}

/// Lightweight metadata about a discovered agent on disk.
#[derive(Debug, Clone)]
pub struct AgentMeta {
    pub id: String,
    pub dir: std::path::PathBuf,
}
