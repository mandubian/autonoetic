//! Gateway configuration types.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Named LLM preset — either a fixed preset (concrete provider/model) or a
/// routing preset (dynamic selection from fixed presets at call time).
/// The two kinds are mutually exclusive.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmPreset {
    // ── Fixed preset fields (mutually exclusive with routing) ──
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub fallback_provider: Option<String>,
    #[serde(default)]
    pub fallback_model: Option<String>,
    /// Set to true if the provider only supports basic chat (no tools at all)
    #[serde(default)]
    pub chat_only: Option<bool>,
    /// Optional context window for CLI "% of context" when preset is applied to SKILL.
    #[serde(default)]
    pub context_window_tokens: Option<u32>,
    /// Optional base URL override for OpenAI-compatible providers (e.g., LM Studio, Ollama).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Optional environment variable name for the API key.
    /// Overrides the provider's default env var (e.g., set to "STREAMLAKE_API_KEY"
    /// for a custom OpenAI-compatible provider instead of the default "OPENAI_API_KEY").
    #[serde(default)]
    pub api_key_env: Option<String>,

    /// Optional extended thinking configuration. When set on a preset, agents
    /// using this preset will inherit the thinking config unless they override
    /// it in their SKILL.md llm_config.
    #[serde(default)]
    pub thinking: Option<crate::agent::ThinkingConfig>,

    // ── Tier/cost (used by fixed presets when referenced by routing presets) ──
    #[serde(default)]
    pub tier: Option<CapabilityTier>,
    #[serde(default)]
    pub cost: Option<ModelCost>,
    #[serde(default)]
    pub latency: Option<ModelLatency>,

    // ── Routing preset fields (mutually exclusive with provider/model) ──
    #[serde(default)]
    pub routing: Option<RoutingPresetConfig>,
}

/// Schema enforcement mode for agent.spawn payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SchemaEnforcementMode {
    /// Disabled - pass through payloads without enforcement.
    Disabled,
    /// Use deterministic coercion (defaults, type coercion).
    #[default]
    Deterministic,
    /// (Later) Use LLM for complex transformations.
    Llm,
}

/// Policy mode for capability-delta gating during revision promotion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityDeltaGateMode {
    /// Any capability broadening requires explicit approval.
    #[default]
    Strict,
    /// Broadening inside an existing wildcard envelope is auto-allowed.
    Evolving,
    /// Disable capability-delta gating (development only).
    Bootstrap,
}

/// Configuration for schema enforcement on agent.spawn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaEnforcementConfig {
    /// Enforcement mode: disabled, deterministic, or llm.
    #[serde(default)]
    pub mode: SchemaEnforcementMode,
    /// Log all enforcement decisions to causal chain.
    #[serde(default = "default_true")]
    pub audit: bool,
    /// Agent-specific overrides (agent_id -> mode).
    #[serde(default)]
    pub agent_overrides: std::collections::HashMap<String, SchemaEnforcementMode>,
}

fn default_true() -> bool {
    true
}

impl Default for SchemaEnforcementConfig {
    fn default() -> Self {
        Self {
            mode: SchemaEnforcementMode::Deterministic,
            audit: true,
            agent_overrides: std::collections::HashMap::new(),
        }
    }
}

/// Session-scoped resource limits enforced by the gateway (role-agnostic).
///
/// All limits are optional: `None` means unlimited for that dimension.
/// Counters are keyed by **session id** (the same id passed to `agent.spawn` / chat),
/// so nested specialist runs in one user session share one budget pool.
///
/// **Related (not duplicated here):** per-agent [`crate::agent::Capability::AgentSpawn`]
/// `max_children` still caps how many child runs a single agent may start per session;
/// configure that on the lead manifest. Future versions may add optional alignment
/// between these knobs via config only.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionBudgetConfig {
    /// Optional profile name for logging and ops (e.g. `dev`, `production`).
    #[serde(default)]
    pub profile: Option<String>,
    /// Maximum LLM `complete()` calls per session (each provider round-trip, including retries).
    #[serde(default)]
    pub max_llm_rounds: Option<u64>,
    /// Maximum tool invocations processed per session (each tool call in a batch counts).
    #[serde(default)]
    pub max_tool_invocations: Option<u64>,
    /// Maximum total LLM tokens (input + output) reported by providers per session.
    #[serde(default)]
    pub max_llm_tokens: Option<u64>,
    /// Maximum wall-clock seconds from first budget touch for this session.
    #[serde(default)]
    pub max_wall_clock_secs: Option<u64>,
    /// Maximum estimated session spend in USD (OpenRouter pricing from the public models API when provider is `openrouter`).
    #[serde(default)]
    pub max_session_price_usd: Option<f64>,
    /// Names of future budget extension modules (reserved; no effect until implemented).
    #[serde(default)]
    pub extensions: Vec<String>,
}

/// Tree-wide budget limits aggregated across all descendants of a root session.
/// Applies in addition to per-session limits; the tighter bound wins.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RootSessionBudgetConfig {
    #[serde(default)]
    pub max_llm_rounds: Option<u64>,
    #[serde(default)]
    pub max_tool_invocations: Option<u64>,
    #[serde(default)]
    pub max_llm_tokens: Option<u64>,
    #[serde(default)]
    pub max_wall_clock_secs: Option<u64>,
    #[serde(default)]
    pub max_session_price_usd: Option<f64>,
}

/// Post-session digest: LLM summarization and Tier-2 memory extraction after agent sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DigestAgentConfig {
    /// When true, run the digest step after eligible sessions complete (spawn / checkpoint resume).
    #[serde(default)]
    pub enabled: bool,
    /// Skip digest when `turn_counter` is strictly below this value at session end.
    #[serde(default = "default_digest_min_turns")]
    pub min_turns: u32,
    /// Use `llm_presets[<name>]` for provider/model/temperature when set.
    #[serde(default)]
    pub llm_preset: Option<String>,
    /// Inline provider when `llm_preset` is not used (e.g. `openai`, `anthropic`).
    #[serde(default)]
    pub provider: Option<String>,
    /// Inline model when `llm_preset` is not used.
    #[serde(default)]
    pub model: Option<String>,
}

fn default_digest_min_turns() -> u32 {
    2
}

impl Default for DigestAgentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_turns: default_digest_min_turns(),
            llm_preset: None,
            provider: None,
            model: None,
        }
    }
}

/// Capability tier for model routing — determines the minimum model quality
/// required for a given task complexity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityTier {
    /// Cheap, fast models (e.g., haiku, gpt-4o-mini). For simple Q&A, classification.
    #[default]
    Economy,
    /// Mid-tier models (e.g., sonnet, gpt-4o). For reasoning + tool use.
    Standard,
    /// Top-tier models (e.g., opus, o1). For complex reasoning, code review.
    Premium,
}

/// Cost configuration for a model entry.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelCost {
    /// Cost per million input tokens in USD.
    #[serde(default)]
    pub input_per_million: Option<f64>,
    /// Cost per million output tokens in USD.
    #[serde(default)]
    pub output_per_million: Option<f64>,
}

/// Latency configuration for a model entry.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelLatency {
    /// Expected time-to-first-token in milliseconds.
    #[serde(default)]
    pub ttft_ms: Option<u64>,
    /// Expected tokens per second output rate.
    #[serde(default)]
    pub tokens_per_second: Option<u64>,
}

/// Budget state for routing decisions.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BudgetState {
    /// Fraction of session budget consumed (0.0–1.0).
    #[serde(default)]
    pub session_budget_used_pct: Option<f32>,
    /// Fraction of prompt budget consumed (0.0–1.0).
    #[serde(default)]
    pub prompt_budget_used_pct: Option<f32>,
    /// Estimated cost of this session so far in USD.
    #[serde(default)]
    pub session_cost_usd: Option<f64>,
}

/// Context for making a routing decision.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoutingContext {
    pub agent_id: String,
    pub session_id: String,
    pub budget: BudgetState,
    pub complexity: ComplexitySignals,
    pub time: TimeSignals,
}

/// Complexity signals for routing decisions.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ComplexitySignals {
    /// Number of tool definitions in the registry.
    #[serde(default)]
    pub tool_count: Option<u32>,
    /// Number of tools used in the last N turns.
    #[serde(default)]
    pub recent_tool_use_count: Option<u32>,
    /// Whether the agent has AgentSpawn capability (workflow orchestration).
    #[serde(default)]
    pub has_workflow_caps: bool,
    /// Whether the agent has WriteAccess capability (artifact generation).
    #[serde(default)]
    pub has_artifact_caps: bool,
    /// Whether the agent is in script mode (no LLM).
    #[serde(default)]
    pub is_script_mode: bool,
}

/// Time signals for routing decisions.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TimeSignals {
    /// Current turn number in the session.
    #[serde(default)]
    pub turn_number: Option<u32>,
    /// Total turns in the session so far.
    #[serde(default)]
    pub session_turn_count: Option<u32>,
    /// Seconds since session start.
    #[serde(default)]
    pub elapsed_secs: Option<u64>,
}

/// Agent-specific model override.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelOverride {
    /// Force a specific model for this agent.
    #[serde(default)]
    pub model: Option<String>,
    /// Minimum tier for this agent.
    #[serde(default)]
    pub min_tier: Option<CapabilityTier>,
}

/// Approval gates for routing decisions.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ApprovalGatesConfig {
    /// Require approval before using premium tier models.
    #[serde(default)]
    pub premium_model_first_use: bool,
    /// Require approval when budget threshold is crossed.
    #[serde(default)]
    pub budget_threshold_crossed: Option<f32>,
}

fn default_budget_downgrade_threshold() -> f32 {
    0.8
}

fn default_max_tier() -> CapabilityTier {
    CapabilityTier::Premium
}

/// Routing strategy selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RoutingStrategy {
    /// Always use the primary model (no routing).
    #[default]
    Disabled,
    /// Deterministic routing based on budget + complexity signals.
    Deterministic,
    /// LLM classifier routing — uses a cheap model to classify complexity.
    Classifier,
    /// Hybrid routing — deterministic first, LLM classifier only when
    /// the deterministic signals are ambiguous.
    Hybrid,
}

/// Configuration for deterministic model routing.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeterministicRoutingConfig {
    /// Maximum capability tier allowed (filters out models above this tier).
    #[serde(default = "default_max_tier")]
    pub max_tier: CapabilityTier,
    /// Maximum cost per session in USD before downgrading to economy tier.
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
    /// Budget pressure threshold (0.0–1.0) at which to downgrade to economy.
    #[serde(default = "default_budget_downgrade_threshold")]
    pub budget_downgrade_threshold: f32,
    /// Whether to include fallback chain on failure.
    #[serde(default = "default_true")]
    pub enable_fallback_chain: bool,
}

/// Routing configuration within a preset. When present, the preset is a
/// dynamic preset that selects from other (fixed) presets at call time.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RoutingPresetConfig {
    pub strategy: RoutingStrategy,
    /// Fixed preset names to route between. Must all be fixed presets.
    #[serde(default)]
    pub models: Vec<String>,
    /// Fixed preset name for the classifier model (classifier/hybrid strategies).
    #[serde(default)]
    pub classifier_preset: Option<String>,
    /// Deterministic strategy settings.
    #[serde(default)]
    pub deterministic: DeterministicRoutingConfig,
    /// Classifier strategy settings.
    #[serde(default)]
    pub classifier: ClassifierRoutingConfig,
    /// Hybrid strategy settings.
    #[serde(default)]
    pub hybrid: HybridRoutingConfig,
}

/// Configuration for LLM classifier routing.
/// When used within a routing preset, the classifier model is resolved
/// from the classifier_preset field rather than from provider/model here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassifierRoutingConfig {
    /// Timeout in seconds for the classifier call (default: 2).
    #[serde(default = "default_classifier_timeout")]
    pub timeout_secs: u64,
    /// Budget pressure threshold (0.0–1.0) at which to skip classifier entirely.
    #[serde(default = "default_classifier_budget_skip")]
    pub skip_threshold: f32,
}

impl Default for ClassifierRoutingConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 2,
            skip_threshold: 0.95,
        }
    }
}

fn default_classifier_timeout() -> u64 {
    2
}

fn default_classifier_budget_skip() -> f32 {
    0.95
}

fn default_ambiguity_threshold() -> f32 {
    0.5
}

/// Configuration for hybrid routing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HybridRoutingConfig {
    /// Ambiguity threshold (0.0–1.0). When deterministic confidence
    /// is below this, the LLM classifier is consulted.
    #[serde(default = "default_ambiguity_threshold")]
    pub ambiguity_threshold: f32,
    /// Classifier settings used when the hybrid router falls through.
    #[serde(default)]
    pub classifier: ClassifierRoutingConfig,
}

impl Default for HybridRoutingConfig {
    fn default() -> Self {
        Self {
            ambiguity_threshold: 0.5,
            classifier: ClassifierRoutingConfig::default(),
        }
    }
}

/// Cross-cutting routing concerns (agent overrides and approval gates).
/// Model definitions and routing strategies now live in `llm_presets`
/// via routing presets.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LlmRoutingConfig {
    /// Agent-specific overrides (agent_id → min_tier or explicit model).
    #[serde(default)]
    pub agent_overrides: std::collections::HashMap<String, ModelOverride>,
    /// Approval gates for routing decisions.
    #[serde(default)]
    pub approval_gates: ApprovalGatesConfig,
}

/// Top-level Gateway daemon configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    /// Directory containing agent subdirectories, each with a SKILL.md.
    #[serde(default = "default_agents_dir")]
    pub agents_dir: PathBuf,

    /// Port for the local JSON-RPC IPC listener.
    #[serde(default = "default_port")]
    pub port: u16,

    /// OFP federation port.
    #[serde(default = "default_ofp_port")]
    pub ofp_port: u16,

    /// Enable TLS on the OFP port.
    #[serde(default)]
    pub tls: bool,

    /// Node identity for OFP federation and causal chain authorship.
    /// Overridable by AUTONOETIC_NODE_ID env var.
    #[serde(default = "default_node_id")]
    pub node_id: String,

    /// Human-readable node name for OFP federation.
    /// Overridable by AUTONOETIC_NODE_NAME env var.
    #[serde(default = "default_node_name")]
    pub node_name: String,

    /// Maximum number of agent runtime executions allowed concurrently.
    #[serde(default = "default_max_concurrent_spawns")]
    pub max_concurrent_spawns: usize,

    /// Maximum number of pending executions admitted per target agent.
    /// This count includes the currently running execution for that agent.
    #[serde(default = "default_max_pending_spawns_per_agent")]
    pub max_pending_spawns_per_agent: usize,

    /// System-wide ceiling for spawn-chain depth (R+3 / R-7.15).
    /// Any agent whose session depth (counting `/` in session_id) equals or exceeds
    /// this value is refused the right to spawn further children.
    /// Per-agent `AgentSpawn.max_spawn_depth` may be lower; the tighter bound wins.
    /// Default: 8.
    #[serde(default = "default_max_spawn_depth")]
    pub max_spawn_depth: u32,

    /// Enable the gateway-owned background scheduler.
    #[serde(default = "default_background_scheduler_enabled")]
    pub background_scheduler_enabled: bool,

    /// Tick interval for background due checks.
    #[serde(default = "default_background_tick_secs")]
    pub background_tick_secs: u64,

    /// Global minimum allowed reevaluation interval across agents.
    #[serde(default = "default_background_min_interval_secs")]
    pub background_min_interval_secs: u64,

    /// Max number of due background agents admitted per scheduler tick.
    #[serde(default = "default_max_background_due_per_tick")]
    pub max_background_due_per_tick: usize,

    /// Schema enforcement configuration for agent.spawn payloads.
    #[serde(default)]
    pub schema_enforcement: SchemaEnforcementConfig,

    /// Named LLM presets for agent bootstrapping (e.g., "agentic" → claude-sonnet).
    #[serde(default)]
    pub llm_presets: HashMap<String, LlmPreset>,

    /// Map role/template names to LLM presets (e.g., "planner" → "agentic", "coder" → "coding").
    #[serde(default)]
    pub llm_preset_mapping: HashMap<String, String>,

    /// Code analysis configuration for agent.install validation.
    /// Controls how the gateway analyzes code for capabilities and security.
    #[serde(default)]
    pub code_analysis: CodeAnalysisConfig,

    /// Capability delta gating mode for `agent.revision.promote`.
    #[serde(default)]
    pub capability_delta_gate_mode: CapabilityDeltaGateMode,

    /// Optional per-session budgets (LLM rounds, tools, tokens, wall clock).
    #[serde(default)]
    pub session_budget: SessionBudgetConfig,

    /// Tree-wide budgets aggregated across all descendants of a root session (R+4 / R-6.21).
    /// Applies in addition to per-session limits; the tighter bound wins.
    #[serde(default)]
    pub root_session_budget: RootSessionBudgetConfig,

    /// Maximum seconds a workflow task may remain in `AwaitingApproval` before it is
    /// automatically marked `Failed`. Set to 0 to disable (not recommended for production).
    /// Default: 600 (10 minutes).
    #[serde(default = "default_approval_timeout_secs")]
    pub approval_timeout_secs: u64,

    /// Maximum number of concurrent pending approvals per root_session_id (R+5 / R-7.17).
    /// When a new approval request would push the count above this cap, the insert is
    /// rejected with `approval_flood`. Set to 0 to disable (not recommended).
    /// Default: 50.
    #[serde(default = "default_max_pending_approvals_per_root")]
    pub max_pending_approvals_per_root: usize,

    /// HMAC-SHA256 key for signing turn continuation files.
    /// This value should be a secret, high-entropy key provided from a secret
    /// source (environment secret or vault); do not derive it from `node_id`
    /// or any other identifier. Rotate by changing this value (existing
    /// continuations will fail integrity verification and be rejected).
    ///
    /// When unset the gateway derives a deterministic key from `node_id`.
    /// This is a development convenience only — it does **not** protect
    /// against a local attacker who can read the config and edit the
    /// continuation file.  Production deployments should set this field.
    #[serde(default)]
    pub continuation_key: Option<String>,

    /// Heartbeat interval (seconds) for workflow tasks in `Running` state.
    ///
    /// Used by both scheduler-driven async runs and synchronous `agent.spawn` waits to
    /// refresh `TaskRun.updated_at` and avoid false stuck-task resolution during long
    /// post-processing tails. If unset, derives from `background_tick_secs` (clamped 1..=5).
    #[serde(default = "default_workflow_task_heartbeat_secs_val")]
    pub workflow_task_heartbeat_secs: Option<u64>,

    /// Maximum seconds a workflow task may remain in `Running` state without progress
    /// before it is automatically force-completed as `Succeeded`. The sweeper checks
    /// whether the child session has actually completed (via session manifest, digest,
    /// or implicit artifact) before resolving. Set to 0 to disable.
    /// Default: 600 (10 minutes).
    #[serde(default = "default_stuck_task_timeout_secs_val")]
    pub stuck_task_timeout_secs: Option<u64>,

    /// Evidence mode configuration.
    /// Controls how much tool/LLM execution data is saved to evidence files for debugging.
    /// "full": all tool results and LLM completions (default for development)
    /// "errors": only failures, approval gates, non-zero exit codes (production recommended)
    /// "off": no evidence files (causal_events DB still captures everything)
    #[serde(default)]
    pub evidence_mode: String,

    /// Optional post-session digest (narrative + extracted memories). Off by default — enable in config.
    #[serde(default)]
    pub digest_agent: DigestAgentConfig,

    /// Data retention settings (days). 0 = retain forever.
    #[serde(default)]
    pub retention: RetentionConfig,

    /// Response validation gate configuration.
    /// When enabled, the gateway validates agent outputs against declared constraints
    /// in agent metadata before returning SpawnResult to the caller.
    #[serde(default)]
    pub response_validation: ResponseValidationConfig,

    /// Sandbox (bubblewrap) isolation overrides.
    /// Env overrides are ignored unless AUTONOETIC_ALLOW_SANDBOX_ENV_OVERRIDES=true.
    #[serde(default)]
    pub sandbox: SandboxConfig,

    /// Maximum number of turns allowed per agent session before forced suspension.
    /// Acts as a circuit breaker for runaway loops. Default: 12.
    #[serde(default = "default_max_session_turns")]
    pub max_session_turns: u32,

    /// Loop guard configuration — per-session circuit breaker for stuck agents.
    #[serde(default)]
    pub loop_guard: LoopGuardConfig,

    /// Prompt budget transparency and enforcement configuration.
    #[serde(default)]
    pub prompt_budget: PromptBudgetConfig,

    /// Optional LLM model routing configuration.
    /// When set, enables intelligent model selection based on budget pressure,
    /// task complexity, and cost constraints.
    #[serde(default)]
    pub llm_routing: Option<LlmRoutingConfig>,

    /// Context compression configuration.
    /// When enabled, the gateway summarizes conversation history when it
    /// exceeds a configurable threshold, replacing old turns with a compact
    /// summary to stay within context limits.
    #[serde(default)]
    pub context_compression: ContextCompressionConfig,

    /// Chat TUI settings.
    #[serde(default)]
    pub chat: ChatConfig,

    /// Approval level / escalation settings.
    #[serde(default)]
    pub approval_levels: ApprovalLevelConfig,

    /// Timeout in seconds for signal delivery (approval resolution, workflow join).
    /// This is the time the signal sender waits for the JSON-RPC response after
    /// delivering an event.ingest to the planner. Planner turns include LLM
    /// inference, so this must be long enough to cover a full turn.
    /// Default: 60.
    #[serde(default = "default_signal_delivery_timeout_secs")]
    pub signal_delivery_timeout_secs: u64,

    #[serde(default)]
    pub hooks: Vec<crate::hooks::HookConfig>,

    /// Scheduled jobs (cron) configuration.
    #[serde(default)]
    pub scheduled_jobs: ScheduledJobsConfig,

    /// System agents: declared background agents that the gateway reconciles on startup.
    #[serde(default)]
    pub system_agents: Vec<SystemAgentEntry>,

    /// When true (default), JSON-RPC `interaction.answer` / `interaction.resolve_and_answer`
    /// persist answers and orchestrate workflow task or session resume (gateway-owned path).
    #[serde(default = "default_interaction_answer_orchestration")]
    pub interaction_answer_orchestration: bool,

    /// Allow sessions to start even when runtime.lock drift is detected (R+7 / R+18).
    /// When true, drift is logged as a causal event but does not block session start.
    /// Default: false (drift is fatal).
    #[serde(default)]
    pub allow_runtime_lock_drift: bool,

    /// When true, allow revision creation to proceed without a gateway signature
    /// when the gateway identity key is unavailable (e.g. first boot on a read-only
    /// filesystem, permission errors). In normal operation the gateway auto-signs every
    /// revision — this flag is only an escape hatch for environments where the key
    /// cannot be loaded or generated. Default: false.
    #[serde(default)]
    pub trust_unsigned_bundles: bool,

    /// Multiplier applied to approval dwell times (R++4). Set to 0 to
    /// disable dwell-time enforcement (for tests). Default: 1.0.
    #[serde(default = "default_approval_dwell_multiplier")]
    pub approval_dwell_multiplier: f64,
}

fn default_approval_dwell_multiplier() -> f64 {
    1.0
}

fn default_interaction_answer_orchestration() -> bool {
    true
}

/// Scheduled jobs (cron) configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledJobsConfig {
    /// Minimum allowed interval between job triggers in seconds.
    /// Prevents abusive high-frequency schedules. Default: 1.
    #[serde(default = "default_scheduled_jobs_min_interval_secs")]
    pub min_interval_secs: u64,

    /// Maximum number of scheduled jobs per root session. Default: 50.
    #[serde(default = "default_scheduled_jobs_max_per_root")]
    pub max_per_root: usize,

    /// Maximum number of due scheduled jobs admitted per scheduler tick. Default: 16.
    #[serde(default = "default_scheduled_jobs_max_due_per_tick")]
    pub max_due_per_tick: usize,
}

impl Default for ScheduledJobsConfig {
    fn default() -> Self {
        Self {
            min_interval_secs: default_scheduled_jobs_min_interval_secs(),
            max_per_root: default_scheduled_jobs_max_per_root(),
            max_due_per_tick: default_scheduled_jobs_max_due_per_tick(),
        }
    }
}

fn default_scheduled_jobs_min_interval_secs() -> u64 {
    1
}

fn default_scheduled_jobs_max_per_root() -> usize {
    50
}

fn default_scheduled_jobs_max_due_per_tick() -> usize {
    16
}

/// A system agent declaration for gateway-managed background execution.
///
/// System agents are reconciled on gateway startup: their cron jobs are
/// created if missing, and they can be manually controlled via CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemAgentEntry {
    /// Agent ID (e.g. "evolution-orchestrator.default").
    pub agent_id: String,
    /// Cron schedule expression (e.g. "0 */4 * * *"). If absent, the agent
    /// is bootstrapped once on startup but not scheduled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    /// Message payload sent to the agent on each trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Whether this system agent is enabled. Disabled agents are skipped
    /// during reconciliation. Default: true.
    #[serde(default = "default_system_agent_enabled")]
    pub enabled: bool,
}

fn default_system_agent_enabled() -> bool {
    true
}

/// Chat TUI configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatConfig {
    /// Allow inline approval of pending requests from the chat TUI (Ctrl+A).
    /// Disabled by default — the approval channel may be separated from chat.
    #[serde(default)]
    pub inline_approvals: bool,
}

/// Approval level / escalation configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ApprovalLevelConfig {
    /// Map action kind → required level. e.g. {"SandboxExec": "admin"}
    /// Omitted = all actions require "operator" (no escalation).
    #[serde(default)]
    pub action_overrides: std::collections::HashMap<String, String>,

    /// Map host pattern → required level. e.g. {"prod-*": "admin"}
    #[serde(default)]
    pub host_overrides: std::collections::HashMap<String, String>,

    /// Default approval level when no override matches. Defaults to "operator".
    #[serde(default)]
    pub default: Option<String>,
}

/// Configuration for evidence storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceConfig {
    /// Evidence mode: "full", "errors", or "off"
    #[serde(default = "default_evidence_mode")]
    pub mode: String,
}

impl Default for EvidenceConfig {
    fn default() -> Self {
        Self {
            mode: "full".to_string(),
        }
    }
}

/// Configuration for data retention / pruning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionConfig {
    /// Days to retain execution_traces. 0 = forever. Default: 30.
    #[serde(default = "default_retention_execution_traces_days")]
    pub execution_traces_days: u32,
    /// Days to retain causal_events. 0 = forever. Default: 90.
    #[serde(default = "default_retention_causal_events_days")]
    pub causal_events_days: u32,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            execution_traces_days: 30,
            causal_events_days: 90,
        }
    }
}

fn default_retention_execution_traces_days() -> u32 {
    30
}
fn default_retention_causal_events_days() -> u32 {
    90
}

/// Configuration for the response validation gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseValidationConfig {
    /// Enable response validation. Default: false (benign until explicitly enabled).
    #[serde(default)]
    pub enabled: bool,

    /// Override mode per invocation: "on" = validate only, "repair" = validate + bounded retry.
    /// Default: use `enabled` flag for "on" behavior; repair requires explicit opt-in.
    #[serde(default)]
    pub repair_enabled: bool,
}

impl Default for ResponseValidationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            repair_enabled: false,
        }
    }
}

/// Sandbox (bubblewrap) isolation overrides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// Share host network namespace (adds --share-net to bwrap).
    /// Env override is ignored unless AUTONOETIC_ALLOW_SANDBOX_ENV_OVERRIDES=true.
    #[serde(default)]
    pub share_net: bool,

    /// /dev mount strategy: "legacy", "minimal", or "host-bind".
    /// Env override is ignored unless AUTONOETIC_ALLOW_SANDBOX_ENV_OVERRIDES=true.
    #[serde(default = "default_sandbox_dev_mode")]
    pub dev_mode: String,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            share_net: false,
            dev_mode: default_sandbox_dev_mode(),
        }
    }
}

fn default_sandbox_dev_mode() -> String {
    "legacy".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopGuardConfig {
    /// Max consecutive LLM rounds without any successful tool call before tripping.
    #[serde(default = "default_max_loops_without_progress")]
    pub max_loops_without_progress: u32,

    /// Max total failures for a single tool name before tripping.
    /// Counts all failures regardless of arguments or targets.
    #[serde(default = "default_max_tool_failures")]
    pub max_tool_failures: u32,

    /// Max consecutive "progress" resets from the same tool+args fingerprint.
    /// After this many identical consecutive tool calls, they stop resetting
    /// current_loops — the agent is spinning on the same operation.
    /// Set to 1 so repeating the same call twice immediately stops counting
    /// as progress (first call resets, second+ call does not).
    #[serde(default = "default_max_consecutive_same_progress")]
    pub max_consecutive_same_progress: u32,

    /// Max child agent task failures before tripping.
    /// Each time workflow.wait returns any_failed:true, the counter increments.
    /// Unlike tool failures, child failures do NOT reset on progress — once a
    /// child task fails, that's a permanent budget hit. Prevents lead agents
    /// from re-spawning failed specialist tasks indefinitely.
    #[serde(default = "default_max_child_failures")]
    pub max_child_failures: u32,
}

impl Default for LoopGuardConfig {
    fn default() -> Self {
        Self {
            max_loops_without_progress: default_max_loops_without_progress(),
            max_tool_failures: default_max_tool_failures(),
            max_consecutive_same_progress: default_max_consecutive_same_progress(),
            max_child_failures: default_max_child_failures(),
        }
    }
}

fn default_max_loops_without_progress() -> u32 {
    5
}

fn default_max_tool_failures() -> u32 {
    5
}

fn default_max_consecutive_same_progress() -> u32 {
    1
}

fn default_max_child_failures() -> u32 {
    3
}

/// Configuration for pluggable code analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeAnalysisConfig {
    /// Provider for capability analysis: "pattern", "python_ast", "llm", "composite", "none"
    #[serde(default = "default_capability_provider")]
    pub capability_provider: String,

    /// Provider for security analysis: "pattern", "python_ast", "llm", "composite", "none"
    #[serde(default = "default_security_provider")]
    pub security_provider: String,

    /// Require capabilities to be declared (reject if missing)
    #[serde(default = "default_require_capabilities")]
    pub require_capabilities: bool,

    /// Capability types that always require human approval when detected
    #[serde(default)]
    pub require_approval_for: Vec<String>,

    /// LLM configuration for LLM-based analysis providers
    #[serde(default)]
    pub llm_config: CodeAnalysisLlmConfig,
}

fn default_capability_provider() -> String {
    "pattern".to_string()
}

fn default_security_provider() -> String {
    "pattern".to_string()
}

fn default_require_capabilities() -> bool {
    true
}

impl Default for CodeAnalysisConfig {
    fn default() -> Self {
        Self {
            capability_provider: default_capability_provider(),
            security_provider: default_security_provider(),
            require_capabilities: default_require_capabilities(),
            require_approval_for: vec!["NetworkAccess".to_string(), "CodeExecution".to_string()],
            llm_config: CodeAnalysisLlmConfig::default(),
        }
    }
}

/// LLM configuration for code analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeAnalysisLlmConfig {
    /// LLM provider for analysis (e.g., "openrouter", "anthropic")
    #[serde(default = "default_analysis_provider")]
    pub provider: String,

    /// Model for code analysis
    #[serde(default = "default_analysis_model")]
    pub model: String,

    /// Temperature (lower = more deterministic)
    #[serde(default = "default_analysis_temperature")]
    pub temperature: f32,

    /// Timeout in seconds
    #[serde(default = "default_analysis_timeout")]
    pub timeout_secs: u64,
}

fn default_analysis_provider() -> String {
    "openrouter".to_string()
}

fn default_analysis_model() -> String {
    "google/gemini-3-flash-preview".to_string()
}

fn default_analysis_temperature() -> f32 {
    0.1
}

fn default_analysis_timeout() -> u64 {
    30
}

impl Default for CodeAnalysisLlmConfig {
    fn default() -> Self {
        Self {
            provider: default_analysis_provider(),
            model: default_analysis_model(),
            temperature: default_analysis_temperature(),
            timeout_secs: default_analysis_timeout(),
        }
    }
}

fn default_agents_dir() -> PathBuf {
    PathBuf::from("./agents")
}

fn default_port() -> u16 {
    4000
}

fn default_ofp_port() -> u16 {
    4200
}

fn default_node_id() -> String {
    "gateway".to_string()
}

fn default_node_name() -> String {
    "gateway".to_string()
}

fn default_max_concurrent_spawns() -> usize {
    8
}

fn default_max_pending_spawns_per_agent() -> usize {
    4
}

fn default_max_spawn_depth() -> u32 {
    8
}

fn default_background_scheduler_enabled() -> bool {
    true
}

fn default_background_tick_secs() -> u64 {
    5
}

fn default_background_min_interval_secs() -> u64 {
    60
}

fn default_max_background_due_per_tick() -> usize {
    32
}

fn default_approval_timeout_secs() -> u64 {
    600
}

fn default_max_pending_approvals_per_root() -> usize {
    50
}

fn default_workflow_task_heartbeat_secs_val() -> Option<u64> {
    None
}

fn default_stuck_task_timeout_secs_val() -> Option<u64> {
    Some(600)
}

fn default_max_session_turns() -> u32 {
    12
}

fn default_signal_delivery_timeout_secs() -> u64 {
    60
}

fn default_evidence_mode() -> String {
    "full".to_string()
}

/// Configuration for prompt budget transparency and enforcement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptBudgetConfig {
    /// Maximum tokens for the system prompt (foundation + agent instructions). 0 = unlimited.
    #[serde(default)]
    pub system_prompt_max_tokens: usize,

    /// Maximum tokens for all tool definitions combined. 0 = unlimited.
    #[serde(default)]
    pub tool_definitions_max_tokens: usize,

    /// Warn when total prompt utilization exceeds this percentage of context window.
    #[serde(default = "default_prompt_budget_warn_pct")]
    pub warn_at_pct: f64,

    /// Reserve this many tokens at the end of the context window for LLM output.
    #[serde(default = "default_prompt_budget_margin")]
    pub margin_tokens: usize,

    /// Action when budget exceeded: "warn" (log only), "trim_history" (remove oldest messages),
    /// "demote_tools" (remove specialized tools), or "fail" (reject the turn).
    #[serde(default)]
    pub on_exceeded: PromptBudgetAction,

    /// Strip tool JSON schemas to `{}` after the first turn to save tokens.
    /// Some LLM providers require full schemas on every request — enable with caution.
    #[serde(default)]
    pub compress_tool_schemas_after_turn_0: bool,
}

/// Action to take when prompt budget is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PromptBudgetAction {
    /// Log a warning but proceed anyway.
    #[default]
    Warn,
    /// Remove oldest non-system messages to fit within budget.
    TrimHistory,
    /// Remove specialized (Specialized tier) tool definitions.
    DemoteTools,
    /// Fail the turn with a budget exceeded error.
    Fail,
}

fn default_prompt_budget_warn_pct() -> f64 {
    80.0
}

fn default_prompt_budget_margin() -> usize {
    4096
}

impl Default for PromptBudgetConfig {
    fn default() -> Self {
        Self {
            system_prompt_max_tokens: 0,
            tool_definitions_max_tokens: 0,
            warn_at_pct: default_prompt_budget_warn_pct(),
            margin_tokens: default_prompt_budget_margin(),
            on_exceeded: PromptBudgetAction::Warn,
            compress_tool_schemas_after_turn_0: false,
        }
    }
}

/// Configuration for context compression.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextCompressionConfig {
    /// Enable context compression. Default: false.
    #[serde(default)]
    pub enabled: bool,

    /// LLM preset name to use for compression (should be a cheap/fast model).
    /// The preset must resolve to a fixed provider/model (not a routing preset).
    #[serde(default)]
    pub llm_preset: Option<String>,

    /// Inline provider (e.g. "anthropic") if not using a preset.
    #[serde(default)]
    pub provider: Option<String>,
    /// Inline model (e.g. "claude-3-haiku-20240307") if not using a preset.
    #[serde(default)]
    pub model: Option<String>,

    /// Compress when conversation tokens exceed this percentage of the context window.
    /// Default: 60.0
    #[serde(default = "default_compression_threshold_pct")]
    pub threshold_pct: f64,

    /// Number of recent turns to always keep in full (not summarized).
    /// Default: 3
    #[serde(default = "default_compression_recent_turns")]
    pub recent_turns_to_keep: usize,

    /// Maximum size of the compressed summary in tokens.
    /// Default: 500
    #[serde(default = "default_compression_max_summary_tokens")]
    pub max_summary_tokens: usize,

    /// Minimum number of turns between compression operations.
    /// Prevents compression thrashing when token count oscillates around
    /// the threshold. Default: 3
    #[serde(default = "default_min_turns_between_compression")]
    pub min_turns_between_compression: u64,
}

fn default_compression_threshold_pct() -> f64 {
    60.0
}

fn default_compression_recent_turns() -> usize {
    3
}

fn default_compression_max_summary_tokens() -> usize {
    500
}

fn default_min_turns_between_compression() -> u64 {
    3
}

impl Default for ContextCompressionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            llm_preset: None,
            provider: None,
            model: None,
            threshold_pct: default_compression_threshold_pct(),
            recent_turns_to_keep: default_compression_recent_turns(),
            max_summary_tokens: default_compression_max_summary_tokens(),
            min_turns_between_compression: default_min_turns_between_compression(),
        }
    }
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            agents_dir: default_agents_dir(),
            port: default_port(),
            ofp_port: default_ofp_port(),
            tls: false,
            node_id: default_node_id(),
            node_name: default_node_name(),
            max_concurrent_spawns: default_max_concurrent_spawns(),
            max_pending_spawns_per_agent: default_max_pending_spawns_per_agent(),
            max_spawn_depth: default_max_spawn_depth(),
            background_scheduler_enabled: default_background_scheduler_enabled(),
            background_tick_secs: default_background_tick_secs(),
            background_min_interval_secs: default_background_min_interval_secs(),
            max_background_due_per_tick: default_max_background_due_per_tick(),
            schema_enforcement: SchemaEnforcementConfig::default(),
            llm_presets: HashMap::new(),
            llm_preset_mapping: HashMap::new(),
            code_analysis: CodeAnalysisConfig::default(),
            capability_delta_gate_mode: CapabilityDeltaGateMode::Strict,
            session_budget: SessionBudgetConfig::default(),
            root_session_budget: RootSessionBudgetConfig::default(),
            approval_timeout_secs: default_approval_timeout_secs(),
            max_pending_approvals_per_root: default_max_pending_approvals_per_root(),
            continuation_key: None,
            workflow_task_heartbeat_secs: default_workflow_task_heartbeat_secs_val(),
            stuck_task_timeout_secs: default_stuck_task_timeout_secs_val(),
            evidence_mode: default_evidence_mode(),
            digest_agent: DigestAgentConfig::default(),
            retention: RetentionConfig::default(),
            response_validation: ResponseValidationConfig::default(),
            sandbox: SandboxConfig::default(),
            max_session_turns: default_max_session_turns(),
            loop_guard: LoopGuardConfig::default(),
            prompt_budget: PromptBudgetConfig::default(),
            llm_routing: None,
            chat: ChatConfig::default(),
            approval_levels: ApprovalLevelConfig::default(),
            context_compression: ContextCompressionConfig::default(),
            signal_delivery_timeout_secs: default_signal_delivery_timeout_secs(),
            hooks: Vec::new(),
            scheduled_jobs: ScheduledJobsConfig::default(),
            system_agents: Vec::new(),
            interaction_answer_orchestration: default_interaction_answer_orchestration(),
            allow_runtime_lock_drift: false,
            trust_unsigned_bundles: false,
            approval_dwell_multiplier: default_approval_dwell_multiplier(),
        }
    }
}

impl GatewayConfig {
    /// Validate that LLM preset references are consistent.
    /// Returns a list of error messages; empty vec means valid.
    pub fn validate_llm_presets(&self) -> Vec<String> {
        let mut errors = Vec::new();

        for (name, preset) in &self.llm_presets {
            let has_provider = preset.provider.is_some();
            let has_model = preset.model.is_some();
            let has_routing = preset.routing.is_some();

            if has_routing && (has_provider || has_model) {
                errors.push(format!(
                    "preset '{}': cannot have both routing and provider/model — they are mutually exclusive",
                    name
                ));
            }
            if !has_routing && (!has_provider || !has_model) {
                errors.push(format!(
                    "preset '{}': fixed presets require both provider and model",
                    name
                ));
            }
            if !has_routing && !has_provider && !has_model {
                errors.push(format!(
                    "preset '{}': must have either routing or provider+model",
                    name
                ));
            }

            if let Some(ref routing) = preset.routing {
                if routing.models.is_empty() {
                    errors.push(format!(
                        "preset '{}': routing preset must have at least one model",
                        name
                    ));
                }
                for model_name in &routing.models {
                    if let Some(mp) = self.llm_presets.get(model_name) {
                        if mp.routing.is_some() {
                            errors.push(format!(
                                "preset '{}': routing.models references routing preset '{}', but only fixed presets are allowed",
                                name, model_name
                            ));
                        }
                    } else {
                        errors.push(format!(
                            "preset '{}': routing.models references unknown preset '{}'",
                            name, model_name
                        ));
                    }
                }
                if let Some(ref cp) = routing.classifier_preset {
                    if let Some(cpp) = self.llm_presets.get(cp) {
                        if cpp.routing.is_some() {
                            errors.push(format!(
                                "preset '{}': classifier_preset '{}' is a routing preset, but must be fixed",
                                name, cp
                            ));
                        }
                    } else {
                        errors.push(format!(
                            "preset '{}': classifier_preset references unknown preset '{}'",
                            name, cp
                        ));
                    }
                }
            }
        }

        for (template, preset_name) in &self.llm_preset_mapping {
            if !self.llm_presets.contains_key(preset_name) {
                errors.push(format!(
                    "llm_preset_mapping: template '{}' references unknown preset '{}'",
                    template, preset_name
                ));
            }
        }

        errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_background_scheduler_defaults() {
        let config = GatewayConfig::default();
        assert!(config.background_scheduler_enabled);
        assert_eq!(config.background_tick_secs, 5);
        assert_eq!(config.background_min_interval_secs, 60);
        assert_eq!(config.max_background_due_per_tick, 32);
    }

    #[test]
    fn session_budget_config_json_roundtrip() {
        let j = serde_json::json!({
            "profile": "staging",
            "max_llm_rounds": 120,
            "max_tool_invocations": 400,
            "max_llm_tokens": 2_000_000u64,
            "max_wall_clock_secs": 7200,
            "extensions": ["future_org_limiter"]
        });
        let parsed: SessionBudgetConfig = serde_json::from_value(j).expect("parse json");
        assert_eq!(parsed.profile.as_deref(), Some("staging"));
        assert_eq!(parsed.max_llm_rounds, Some(120));
        assert_eq!(parsed.max_tool_invocations, Some(400));
        assert_eq!(parsed.max_llm_tokens, Some(2_000_000));
        assert_eq!(parsed.max_wall_clock_secs, Some(7200));
        assert_eq!(parsed.extensions, vec!["future_org_limiter"]);
    }

    #[test]
    fn prompt_budget_config_defaults() {
        let config = GatewayConfig::default();
        assert_eq!(config.prompt_budget.system_prompt_max_tokens, 0);
        assert_eq!(config.prompt_budget.tool_definitions_max_tokens, 0);
        assert_eq!(config.prompt_budget.warn_at_pct, 80.0);
        assert_eq!(config.prompt_budget.margin_tokens, 4096);
        assert_eq!(config.prompt_budget.on_exceeded, PromptBudgetAction::Warn);
    }

    #[test]
    fn prompt_budget_config_json_roundtrip() {
        let j = serde_json::json!({
            "system_prompt_max_tokens": 8000,
            "tool_definitions_max_tokens": 4000,
            "warn_at_pct": 90.0,
            "margin_tokens": 2048,
            "on_exceeded": "demote_tools"
        });
        let parsed: PromptBudgetConfig = serde_json::from_value(j).expect("parse json");
        assert_eq!(parsed.system_prompt_max_tokens, 8000);
        assert_eq!(parsed.tool_definitions_max_tokens, 4000);
        assert_eq!(parsed.warn_at_pct, 90.0);
        assert_eq!(parsed.margin_tokens, 2048);
        assert_eq!(parsed.on_exceeded, PromptBudgetAction::DemoteTools);
    }

    #[test]
    fn validate_llm_presets_accepts_valid_config() {
        let mut config = GatewayConfig::default();
        config.llm_presets.insert(
            "haiku".to_string(),
            LlmPreset {
                provider: Some("anthropic".to_string()),
                model: Some("claude-haiku-3".to_string()),
                temperature: None,
                fallback_provider: None,
                fallback_model: None,
                chat_only: None,
                context_window_tokens: None,
                base_url: None,
                api_key_env: None,
                thinking: None,
                tier: Some(CapabilityTier::Economy),
                cost: None,
                latency: None,
                routing: None,
            },
        );
        config.llm_presets.insert(
            "smart".to_string(),
            LlmPreset {
                provider: None,
                model: None,
                temperature: None,
                fallback_provider: None,
                fallback_model: None,
                chat_only: None,
                context_window_tokens: None,
                base_url: None,
                api_key_env: None,
                thinking: None,
                tier: None,
                cost: None,
                latency: None,
                routing: Some(RoutingPresetConfig {
                    strategy: RoutingStrategy::Deterministic,
                    models: vec!["haiku".to_string()],
                    classifier_preset: Some("haiku".to_string()),
                    deterministic: DeterministicRoutingConfig::default(),
                    classifier: ClassifierRoutingConfig::default(),
                    hybrid: HybridRoutingConfig::default(),
                }),
            },
        );
        config
            .llm_preset_mapping
            .insert("planner".to_string(), "smart".to_string());

        let errors = config.validate_llm_presets();
        assert!(errors.is_empty(), "unexpected errors: {:?}", errors);
    }

    #[test]
    fn validate_llm_presets_rejects_routing_with_provider() {
        let mut config = GatewayConfig::default();
        config.llm_presets.insert(
            "bad".to_string(),
            LlmPreset {
                provider: Some("openai".to_string()),
                model: Some("gpt-4".to_string()),
                temperature: None,
                fallback_provider: None,
                fallback_model: None,
                chat_only: None,
                context_window_tokens: None,
                base_url: None,
                api_key_env: None,
                thinking: None,
                tier: None,
                cost: None,
                latency: None,
                routing: Some(RoutingPresetConfig {
                    strategy: RoutingStrategy::Deterministic,
                    models: vec![],
                    classifier_preset: None,
                    deterministic: DeterministicRoutingConfig::default(),
                    classifier: ClassifierRoutingConfig::default(),
                    hybrid: HybridRoutingConfig::default(),
                }),
            },
        );

        let errors = config.validate_llm_presets();
        assert!(errors.iter().any(|e| e.contains("mutually exclusive")));
    }

    #[test]
    fn validate_llm_presets_rejects_fixed_without_provider() {
        let mut config = GatewayConfig::default();
        config.llm_presets.insert(
            "incomplete".to_string(),
            LlmPreset {
                provider: None,
                model: None,
                temperature: None,
                fallback_provider: None,
                fallback_model: None,
                chat_only: None,
                context_window_tokens: None,
                base_url: None,
                api_key_env: None,
                thinking: None,
                tier: None,
                cost: None,
                latency: None,
                routing: None,
            },
        );

        let errors = config.validate_llm_presets();
        assert!(errors
            .iter()
            .any(|e| e.contains("require both provider and model")));
    }

    #[test]
    fn validate_llm_presets_rejects_routing_preset_in_models() {
        let mut config = GatewayConfig::default();
        config.llm_presets.insert(
            "router".to_string(),
            LlmPreset {
                provider: None,
                model: None,
                temperature: None,
                fallback_provider: None,
                fallback_model: None,
                chat_only: None,
                context_window_tokens: None,
                base_url: None,
                api_key_env: None,
                thinking: None,
                tier: None,
                cost: None,
                latency: None,
                routing: Some(RoutingPresetConfig {
                    strategy: RoutingStrategy::Deterministic,
                    models: vec!["router".to_string()], // self-reference
                    classifier_preset: None,
                    deterministic: DeterministicRoutingConfig::default(),
                    classifier: ClassifierRoutingConfig::default(),
                    hybrid: HybridRoutingConfig::default(),
                }),
            },
        );

        let errors = config.validate_llm_presets();
        assert!(errors
            .iter()
            .any(|e| e.contains("only fixed presets are allowed")));
    }

    #[test]
    fn validate_llm_presets_rejects_unknown_preset_in_mapping() {
        let mut config = GatewayConfig::default();
        config
            .llm_preset_mapping
            .insert("coder".to_string(), "nonexistent".to_string());

        let errors = config.validate_llm_presets();
        assert!(errors.iter().any(|e| e.contains("unknown preset")));
    }
}
