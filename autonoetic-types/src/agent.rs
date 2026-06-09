//! Agent Manifest types — the Rust representation of `SKILL.md` frontmatter.

use crate::background::BackgroundPolicy;
use crate::disclosure::DisclosurePolicy;
use serde::{Deserialize, Serialize};

use crate::capability::Capability;

fn default_version() -> String {
    "1.0".to_string()
}

fn is_default_script_input_mode(mode: &ScriptInputMode) -> bool {
    matches!(mode, ScriptInputMode::Stdin)
}

fn is_default_remote_access_approval_mode(mode: &RemoteAccessApprovalMode) -> bool {
    matches!(mode, RemoteAccessApprovalMode::Required)
}

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
    /// Optional base URL override for OpenAI-compatible providers (e.g., LM Studio, Ollama).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Optional env var name for the API key. Overrides the provider's default.
    /// E.g., set to "STREAMLAKE_API_KEY" for a custom OpenAI-compatible provider
    /// instead of the default "OPENAI_API_KEY".
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// When set, this agent's LLM is resolved via the named routing preset
    /// at call time. provider/model are the fallback if routing is unavailable.
    #[serde(default)]
    pub routing_preset: Option<String>,
    /// Extended thinking configuration. When set, enables the model's native
    /// reasoning mode (e.g., o-series reasoning_effort, Anthropic thinking budget,
    /// Gemma <|think|> token). The gateway translates this to provider-native format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
}

/// Extended thinking / reasoning configuration for models that support it.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ThinkingConfig {
    /// Reasoning effort level: "low", "medium", or "high".
    /// For OpenAI o-series: maps to reasoning_effort.
    /// For Anthropic: controls budget_tokens allocation.
    /// For Gemma: enables <|think|> token when truthy.
    #[serde(default)]
    pub effort: ThinkingEffort,
    /// Override for Anthropic-style thinking budget (max tokens for reasoning).
    /// If unset, defaults to 50% of max_tokens or a provider-specific default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingEffort {
    Low,
    #[default]
    Medium,
    High,
    /// Maximum reasoning. Providers that expose a distinct top tier (e.g.
    /// OpenRouter / DeepSeek with `"xhigh"`) get the literal value; providers
    /// whose API only accepts `low|medium|high` (e.g. OpenAI o-series)
    /// collapse this to `"high"`.
    #[serde(rename = "xhigh")]
    XHigh,
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
    /// Reasoning tokens (subset of `output_tokens`) when the provider reports
    /// them. 0/omitted when unknown or not a reasoning model.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub reasoning_tokens: u64,
    /// Prompt tokens served from cache (subset of `input_tokens`) when the
    /// provider reports them. 0/omitted when unknown or caching is disabled.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cached_tokens: u64,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
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
    #[serde(default = "default_version")]
    pub version: String,
    pub runtime: RuntimeDeclaration,
    pub agent: AgentIdentity,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    /// Named preset in gateway `llm_presets` (preferred over inline provider/model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_preset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_config: Option<LlmConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<ResourceLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<BackgroundPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disclosure: Option<DisclosurePolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io: Option<AgentIO>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub middleware: Option<Middleware>,
    /// Execution mode: Script (fast path, no LLM) or Reasoning (default, LLM-driven).
    #[serde(default)]
    pub execution_mode: ExecutionMode,
    /// Entry script for Script mode. Relative path from agent directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_entry: Option<String>,
    /// How input is delivered to script agents: stdin (default) or args ($1).
    #[serde(default, skip_serializing_if = "is_default_script_input_mode")]
    pub script_input_mode: ScriptInputMode,
    /// Remote gateway URL for distributed agents. When set, SDK uses HTTP mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_url: Option<String>,
    /// Authentication token for remote gateway (Bearer token).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_token: Option<String>,
    /// Tool tiers this agent is allowed to use. Empty means all tiers.
    /// When set, tools outside these tiers are excluded from the agent's tool set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_tool_tiers: Vec<ToolTier>,
    /// Metadata from AgentSkills.io import. Set when the agent was imported
    /// from an external AgentSkills-compatible SKILL.md. Used for tool name
    /// bridging, resource mounting, and trust mode enforcement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agentskills_import: Option<AgentSkillsImportMetadata>,
    /// Per-agent context compression override. When present, enables compression
    /// for this agent with optional overrides for threshold and LLM preset.
    #[serde(default)]
    pub compression: Option<CompressionConfig>,
    /// Sandbox network egress policy (RFC scope 5.1). Default `Normal`.
    ///
    /// `Sealed` routes HTTP through a fixture-driven proxy (scope 5.2
    /// shipped — proxy + fixture loader + advisory env injection). HTTPS
    /// is not yet supported (scope 5.2d deferred).
    ///
    /// `Recording` is a dormant stub — the proxy treats it identically to
    /// `Sealed`. Live-capture on fixture miss (scope 5.3) is not yet
    /// implemented. Declaring `Recording` produces a refuse-boot unless
    /// `gateway.sandbox.allow_recording` is set.
    #[serde(default, skip_serializing_if = "is_default_sandbox_network_policy")]
    pub sandbox_network: SandboxNetworkPolicy,
}

fn is_default_sandbox_network_policy(p: &SandboxNetworkPolicy) -> bool {
    matches!(p, SandboxNetworkPolicy::Normal)
}

/// Agent-declared remote-access patterns used for deterministic gateway checks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteAccessDeclaration {
    /// How remote signals are handled by sandbox approval flow.
    /// `required` means operator approval is required for remote execution.
    /// `preapproved` allows auto-approval when the agent has NetworkAccess capability.
    #[serde(
        default,
        skip_serializing_if = "is_default_remote_access_approval_mode"
    )]
    pub approval_mode: RemoteAccessApprovalMode,
    /// Declarative network targets for outbound access.
    /// Supports any-host, exact host, host suffix, host+port, and URL prefix rules.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<RemoteAccessTarget>,
    /// Optional language-detector allowlist for import scanning.
    /// Empty means all registered language detectors are enabled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_languages: Vec<RemoteAccessLanguage>,
    /// Python import/module patterns expected in network-capable code.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub python_imports: Vec<String>,
    /// JavaScript/TypeScript import/module patterns expected in network-capable code.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub js_imports: Vec<String>,
    /// Rust import/module patterns expected in network-capable code.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rust_imports: Vec<String>,
    /// Go import/module patterns expected in network-capable code.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub go_imports: Vec<String>,
    /// Function/method call patterns expected in network-capable code.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub function_calls: Vec<String>,
    /// Shell command patterns expected for remote/network operations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shell_commands: Vec<String>,
    /// Package-manager command patterns (pip/npm/apt/etc.) expected by the agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub package_manager_commands: Vec<String>,
}

/// Optional per-agent LoopGuard limits declared in SKILL metadata.
///
/// Gateway applies these as stricter bounds within system ceilings.
/// Any declared value above system config is capped to the system value.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoopGuardDeclaration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_loops_without_progress: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_failures: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_consecutive_same_progress: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_child_failures: Option<u32>,
}

/// Language detector identifier for remote-access import scanning.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAccessLanguage {
    Python,
    Javascript,
    Rust,
    Go,
}

/// How sandbox.exec handles approval for detected remote/network behavior.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAccessApprovalMode {
    /// Every remote execution requires explicit operator approval.
    #[default]
    Required,
    /// Remote execution can auto-proceed when coarse capability allows network use.
    Preapproved,
}

/// Typed target declarations for `remote_access`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum RemoteAccessTarget {
    /// Match any outbound host/URL target.
    Any,
    /// Exact hostname match, e.g. `"api.github.com"`.
    ExactHost(String),
    /// Matches any subdomain of the suffix, e.g. `"*.github.com"`.
    HostSuffix(String),
    /// Exact host + port, e.g. `{"host":"api.github.com","port":443}`.
    HostAndPort { host: String, port: u16 },
    /// Matches URLs starting with this prefix, e.g.
    /// `"https://api.github.com/public/"`.
    UrlPrefix(String),
}

/// Per-agent context compression configuration (opt-in via SKILL.md).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CompressionConfig {
    /// Override the gateway-level compression threshold percentage.
    #[serde(default)]
    pub threshold_pct: Option<f64>,
    /// Override the gateway-level LLM preset for compression.
    #[serde(default)]
    pub llm_preset: Option<String>,
    /// Override the number of recent turns to keep.
    #[serde(default)]
    pub recent_turns_to_keep: Option<usize>,
    /// Override max capsule decisions (capsule strategy only).
    #[serde(default)]
    pub max_capsule_decisions: Option<usize>,
    /// Override max completed tasks (capsule strategy only).
    #[serde(default)]
    pub max_completed_tasks: Option<usize>,
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

/// How the gateway delivers normalized task input to a script-mode agent.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScriptInputMode {
    /// Write the normalized payload to the script's stdin (default).
    #[default]
    Stdin,
    /// Pass the normalized payload as the first positional CLI argument ($1).
    Args,
}

/// Tool tier for progressive disclosure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolTier {
    /// Always available: content, knowledge basics, artifact basics, sandbox.
    Core,
    /// Workflow-dependent: agent, workflow, evaluation, approval.
    Workflow,
    /// Specialized: web search, promotion, advanced revision ops.
    Specialized,
}

/// Sandbox network egress policy — declared per agent in the SKILL.md
/// frontmatter as `metadata.autonoetic.sandbox_network`. See the
/// sealed-network sandbox design (RFC scope 5.1).
///
/// - `Normal` (default): outbound HTTP/DNS follow the existing capability
///   checks and remote-access approval flow. No interception.
/// - `Sealed`: every outbound HTTP/DNS attempt is intercepted at the
///   sandbox egress layer and routed to a fixture responder. Hits return
///   canned responses; misses return a structured `unfixtured_target`
///   error. Live network is never reached.
/// - `Recording`: like `Sealed`, but on a fixture miss the request is
///   sent live and the response captured as a new fixture. This is a
///   developer/operator-only mode and is **operator-gated** at the
///   gateway: a session whose manifest declares `Recording` refuses to
///   start unless `gateway.sandbox.allow_recording` is `true`.
///
/// `Sealed` and `Recording` are dormant until RFC scopes 5.2 (egress hook)
/// and 5.3 (recording machinery) ship. Until then, declaring them in a
/// manifest parses successfully but has no runtime effect beyond the
/// refuse-boot guard on `Recording`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SandboxNetworkPolicy {
    #[default]
    Normal,
    Sealed,
    Recording,
}

/// Session runtime state — declares the session's purpose at start and tracks
/// runtime health transitions.
///
/// `Clarification` is a first-class declared purpose, not a degradation:
/// it is set at session start (typically by `spawn_clarification_for_approval`)
/// and stays for the life of the session. Tool tier is clamped read-only by
/// the tier filter — agents in clarification sessions can only inspect, never
/// act. See `docs/design/human-gate-unification-plan.md` §Phase 5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    #[default]
    Normal,
    Degraded,
    Clarification,
}

/// A stored credential record for agent-to-service authentication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialRecord {
    /// Unique credential identifier.
    pub credential_id: String,
    /// Service name (e.g. "github", "stripe", "slack").
    pub service: String,
    /// Secret name/key in the vault for the access token.
    pub secret_name: String,
    /// Environment variable or header name to inject as.
    pub inject_as: Option<String>,
    /// Agent that originally created this credential.
    pub created_by_agent: Option<String>,
    /// Optional expiry timestamp (ISO 8601).
    pub expires_at: Option<String>,
    /// Optional label distinguishing multiple credentials for the same service.
    /// E.g. "agent-a", "agent-b". When absent, the credential is unlabeled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Agents this credential is shared with.
    #[serde(default)]
    pub shared_with: Vec<String>,
    /// Hosts this credential is bound to (prevents exfiltration to unrelated hosts).
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Secret name/key in the vault for the refresh token.
    #[serde(default)]
    pub refresh_token_secret_name: Option<String>,
    /// URL to call for token refresh (e.g. "https://oauth.provider.com/token").
    #[serde(default)]
    pub refresh_url: Option<String>,
    /// HTTP method for refresh (default: POST).
    #[serde(default)]
    pub refresh_method: Option<String>,
    /// Static headers for the refresh request (e.g. Content-Type).
    #[serde(default)]
    pub refresh_headers: Option<std::collections::HashMap<String, String>>,
    /// JSON path to extract the new access token from the refresh response (e.g. "$.access_token").
    #[serde(default)]
    pub refresh_extract_access_token: Option<String>,
    /// JSON path to extract a new refresh token from the refresh response (if rotation).
    #[serde(default)]
    pub refresh_extract_refresh_token: Option<String>,
    /// JSON path to extract the new expiry from the refresh response (e.g. "$.expires_in").
    #[serde(default)]
    pub refresh_extract_expires_in: Option<String>,
}

/// A step in the credential setup (automated registration) workflow.
/// Each step is executed server-side by the gateway during `credential.setup`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "step_type", rename_all = "snake_case")]
pub enum CredentialSetupStep {
    /// Gateway makes an HTTP call, extracts secrets from the response,
    /// stores them in the vault, and returns public fields to the agent.
    ApiCall {
        method: Option<String>,
        url: String,
        #[serde(default)]
        headers: std::collections::HashMap<String, String>,
        body: Option<serde_json::Value>,
        /// Secret fields to extract and store in vault: secret_name -> JSONPath
        #[serde(default)]
        extract_secrets: std::collections::HashMap<String, String>,
        /// Public fields to extract and return to agent: field_name -> JSONPath
        #[serde(default)]
        extract_public: std::collections::HashMap<String, String>,
    },
    /// Gateway prompts a human through a secure out-of-band channel for secret values.
    UserPrompt {
        message: String,
        secret_fields: Vec<SecretFieldSpec>,
    },
    /// Gateway instructs the agent to perform an action (e.g. visit a URL).
    UserAction {
        instruction: String,
        #[serde(default)]
        data_refs: Vec<String>,
    },
    /// Tells `credential.setup` to pause and ask the agent to collect user input.
    ///
    /// When the gateway encounters this step it returns early with
    /// `suspended_for_user_input: true` and the question.  The agent should call
    /// `user.ask` with the question, collect the human's answer, then call
    /// `credential.setup` again with `credential_id` + `resume_vars: { var_name: answer }`.
    UserInput { question: String, var_name: String },
}

/// Specification for a secret field in a UserPrompt step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretFieldSpec {
    /// Field name used as the secret name in the vault.
    pub name: String,
    /// Human-readable label shown in the prompt.
    pub label: String,
    /// Whether to mask input (password-style).
    #[serde(default)]
    pub masked: bool,
}

/// Enforcement mode for `io.returns` schema validation.
///
/// - `strict` (default for script agents): validation failures block the response.
/// - `advisory` (default for LLM agents): violations are logged and emitted as
///   causal events but do NOT block the response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IoReturnsEnforcement {
    #[default]
    Strict,
    Advisory,
}

impl IoReturnsEnforcement {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Advisory => "advisory",
        }
    }
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
    /// Enforcement mode for io.returns validation.
    ///
    /// When unset, the gateway resolves the default based on execution_mode:
    /// script → strict, llm/reasoning → advisory.
    #[serde(default)]
    pub returns_enforcement: Option<IoReturnsEnforcement>,
    /// Gateway-enforced output policy (non-schema runtime constraints).
    #[serde(default)]
    pub output_policy: Option<OutputPolicy>,
}

impl AgentIO {
    /// Resolve the effective enforcement mode for io.returns.
    ///
    /// If `returns_enforcement` is explicitly set, use that.
    /// Otherwise, default to `Advisory` for reasoning agents, `Strict` for script agents.
    pub fn effective_returns_enforcement(&self, execution_mode: ExecutionMode) -> IoReturnsEnforcement {
        self.returns_enforcement.unwrap_or_else(|| match execution_mode {
            ExecutionMode::Script => IoReturnsEnforcement::Strict,
            ExecutionMode::Reasoning => IoReturnsEnforcement::Advisory,
        })
    }
}

/// Gateway-enforced output policy declared in manifest metadata.
///
/// When present, the gateway validates the agent's SpawnResult against these
/// constraints before returning to the caller. Violations trigger a ToolError
/// with a repair hint; the agent may retry within bounded loop/duration limits.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OutputPolicy {
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

    /// Auto-repair policy. Disabled by default; agents must opt in explicitly.
    #[serde(default)]
    pub repair: OutputPolicyRepairPolicy,
}

/// Agent-declared policy for gateway-side response auto-repair.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OutputPolicyRepairPolicy {
    /// Whether gateway auto-repair is enabled for this agent.
    #[serde(default)]
    pub auto: bool,
    /// Max auto-repair attempts requested by the agent.
    /// Attempts are additionally capped by gateway-level ceiling.
    #[serde(default)]
    pub max_attempts: Option<u32>,
}

fn default_validation_max_loops() -> u32 {
    1
}

fn default_validation_max_duration_ms() -> u64 {
    500
}

impl OutputPolicy {
    /// Clamp loop/duration bounds to allowed ranges.
    pub fn normalize(&mut self) {
        self.validation_max_loops = self.validation_max_loops.clamp(1, 8);
        self.validation_max_duration_ms = self.validation_max_duration_ms.clamp(0, 30_000);
        if let Some(n) = self.repair.max_attempts {
            self.repair.max_attempts = Some(n.clamp(0, 8));
        }
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
            && self.prohibited_text_patterns.is_empty()
            && self.min_artifact_builds.is_none()
    }

    /// Resolve the declared repair attempts.
    pub fn declared_repair_attempts(&self) -> usize {
        if let Some(max_attempts) = self.repair.max_attempts {
            return max_attempts as usize;
        }
        self.validation_max_loops.saturating_sub(1) as usize
    }
}

/// Lightweight metadata about a discovered agent on disk.
#[derive(Debug, Clone)]
pub struct AgentMeta {
    pub id: String,
    pub dir: std::path::PathBuf,
}

/// A stored user profile for cross-session personalization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserProfileRecord {
    /// Unique user identifier.
    pub user_id: String,
    /// Human-readable display name.
    pub display_name: Option<String>,
    /// Trust domain: `local`, `partner`, `foreign`, `untrusted`.
    pub trust_domain: String,
    /// Origin node for federation provenance.
    pub origin_node_id: Option<String>,
    /// Arbitrary JSON blob containing profile data (preferences, constraints, context).
    pub profile_json: Option<String>,
    /// Monotonically increasing version for optimistic concurrency.
    pub profile_version: i64,
    /// ISO 8601 creation timestamp.
    pub created_at: String,
    /// ISO 8601 last-update timestamp.
    pub updated_at: String,
}

/// Scope of visibility an agent has into a user's profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BindingScope {
    /// Full profile injected into system prompt.
    Full,
    /// Only preferences and constraints injected.
    Restricted,
    /// No profile data injected (binding exists but no wake injection).
    TaskOnly,
}

impl std::fmt::Display for BindingScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => write!(f, "full"),
            Self::Restricted => write!(f, "restricted"),
            Self::TaskOnly => write!(f, "task_only"),
        }
    }
}

/// A binding between a user and an agent, controlling profile visibility scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserAgentBinding {
    /// The user whose profile is accessible.
    pub user_id: String,
    /// The agent that can access the profile.
    pub agent_id: String,
    /// What level of profile data the agent can see.
    pub scope: BindingScope,
    /// ISO 8601 timestamp when the binding was granted.
    pub granted_at: String,
    /// Who approved the binding (user, admin, or agent via approval queue).
    pub granted_by: Option<String>,
}

/// Metadata attached when an agent is imported from the AgentSkills.io format.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentSkillsImportMetadata {
    /// Original license from the AgentSkills frontmatter.
    pub license: Option<String>,
    /// Compatibility string (e.g., "claude-code", "cursor", "copilot").
    pub compatibility: Option<String>,
    /// Raw allowed-tools list from the original frontmatter.
    pub allowed_tools: Vec<String>,
    /// Whether tool name bridging should be injected into the system prompt.
    pub needs_tool_bridging: bool,
}
