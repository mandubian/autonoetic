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
    /// Use deterministic coercion only (defaults, type coercion).
    #[default]
    Deterministic,
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

/// Smoke-test gate mode for new agent installation.
///
/// **Deprecated (#578):** promotion now requires smoke tests unconditionally for
/// capability-bearing agents (`NetworkAccess` / `CodeExecution`). This field is
/// retained for backwards-compatible YAML only and has no effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentInstallSmokeTestMode {
    /// Ask the operator whether to run a smoke test before promoting.
    /// If the operator declines or the test fails, the agent is not installed.
    #[default]
    Ask,
    /// Require a successful smoke test for every new agent promotion.
    /// `agent_revision_promote` will reject the promotion without evidence.
    Required,
    /// Skip the smoke-test gate (legacy behavior).
    Skip,
}

/// Configuration for schema enforcement on agent.spawn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaEnforcementConfig {
    /// Enforcement mode: disabled or deterministic.
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

fn default_plan_auto_approver() -> String {
    "auto-approve".to_string()
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

/// Structured live/session report files under `.gateway/sessions/<id>/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionReportConfig {
    /// When true, rewrite `session_overview.html` on every report update.
    /// Default `false` — HTML is written on session close only (lower CPU/IO).
    #[serde(default)]
    pub live_html_on_update: bool,
}

impl Default for SessionReportConfig {
    fn default() -> Self {
        Self {
            live_html_on_update: false,
        }
    }
}

impl Default for DigestAgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_turns: default_digest_min_turns(),
            llm_preset: None,
            provider: None,
            model: None,
        }
    }
}

/// Outcome grader: optional independent LLM-graded `Completion` verdict
/// attached to each session's `SessionOutcome` row after the post-session
/// digest runs. Self-Improvement loop P0 (#245). The grader must NOT be
/// the agent that ran the session (ownership invariant).
///
/// **Two gates** must be true for grading to run: this struct's
/// `enabled` AND the top-level `auto_learning.enabled` master switch.
/// The auto-learning gate lets an operator mute the whole
/// digest+grading+memory pipeline in one place; setting
/// `outcome_grader.enabled = true` while `auto_learning.enabled = false`
/// is a no-op (logged at the writer layer but silent at startup).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeGraderConfig {
    /// When true AND `auto_learning.enabled` is true, run the grader
    /// after eligible sessions complete. The auto-populated
    /// `SessionOutcome` row (cost/tokens/turns/wall) is still written
    /// regardless of either flag — only the LLM-graded `Completion`
    /// field is gated.
    #[serde(default = "default_outcome_grader_enabled")]
    pub enabled: bool,
    /// Skip grading when `turn_counter` is strictly below this value at
    /// session end (short sessions are usually too thin to grade).
    #[serde(default = "default_outcome_grader_min_turns")]
    pub min_turns: u32,
    /// Agent ID of the grader bundle. Must differ from the run agent's
    /// ID at write time. Default: `outcome-grader.default`.
    #[serde(default = "default_outcome_grader_agent_id")]
    pub grader_agent_id: String,
}

fn default_outcome_grader_enabled() -> bool {
    // Default OFF for safety. Operators opt in by setting
    // `outcome_grader.enabled: true` in their gateway config. This
    // matches the conservative default chosen for the trajectory
    // monitor and lets existing deployments pick up the schema
    // (auto-populated metrics) without paying any LLM cost.
    false
}

fn default_outcome_grader_min_turns() -> u32 {
    2
}

fn default_outcome_grader_agent_id() -> String {
    "outcome-grader.default".to_string()
}

impl Default for OutcomeGraderConfig {
    fn default() -> Self {
        Self {
            enabled: default_outcome_grader_enabled(),
            min_turns: default_outcome_grader_min_turns(),
            grader_agent_id: default_outcome_grader_agent_id(),
        }
    }
}

/// Self-improvement loop runtime guardrails. P4 (#249) shipped the
/// prompt-only safety posture; P5 (#250) lifts it selectively for
/// agent-level changes (capability set / routing / sub-agent topology)
/// behind stronger defenses.
///
/// The gate at A/B replay time is now a three-state policy:
///
/// 1. **No capability delta** → proceed normally with the
///    caller-supplied holdout (typically 0.3).
/// 2. **Capability delta, but operator has not opted in** → reject
///    (this is the P4 behaviour, still the default).
/// 3. **Capability delta + opted in + low blast radius** → proceed
///    with a **minimum holdout of `capability_change_min_holdout`**
///    (default 0.5). The holdout is coerced up if the caller
///    requested a lower value, with the coercion logged in the
///    response.
/// 4. **Capability delta + opted in + HIGH blast radius** → reject.
///    Changes that broaden sandbox / network / code-execution /
///    credential / scheduler / agent-revision capabilities are never
///    eligible for the automated path. Operators promote those
///    through the existing P-2.16 constitutional gate by hand.
///
/// `restrict_to_prompt_only` is the master switch. When `false`, the
/// policy short-circuits to "always allow" (no defenses). Keep it
/// `true` unless you genuinely want every capability change to skip
/// the gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImproveConfig {
    /// When true (default), the surface-change policy above is
    /// enforced at `improvement.ab_replay` time. Set to `false` only
    /// after the loop has earned enough track record that you trust
    /// it to evaluate capability changes without the safety
    /// scaffolding — this is the "raw operator-driven path" for P7
    /// auto-approve.
    #[serde(default = "default_improve_restrict_to_prompt_only")]
    pub restrict_to_prompt_only: bool,

    /// P5: when true, the gate **allows** A/B comparisons where the
    /// candidate has a non-empty capability delta vs the baseline,
    /// provided the change is not high-blast-radius. Default `false`
    /// — operators opt in per-deployment after the prompt-only loop
    /// has banked some P4 track record.
    #[serde(default = "default_improve_allow_capability_changes")]
    pub allow_capability_changes: bool,

    /// P5: minimum holdout ratio enforced when the comparison
    /// involves a capability change. The tool coerces the caller's
    /// `holdout_ratio` up to this value (and notes the coercion in
    /// the response) rather than rejecting outright — capability
    /// changes are inherently more likely to break tasks the
    /// proposal wasn't optimized against, so a wider safety net is
    /// the right default. Range: `[0.0, 1.0]`. Default `0.5`.
    #[serde(default = "default_improve_capability_change_min_holdout")]
    pub capability_change_min_holdout: f64,

    /// P5: capability kinds whose addition or broadening is treated
    /// as high-blast-radius and rejected even when
    /// `allow_capability_changes` is true. The list reflects
    /// "privileges whose widening breaks the sandbox or
    /// reaches the network / shell / credentials / scheduler / agent
    /// promotion". An operator who wants to push such a change still
    /// can — by promoting the revision manually through the P-2.16
    /// gate — but the automated loop refuses.
    #[serde(default = "default_improve_high_blast_radius_capability_kinds")]
    pub high_blast_radius_capability_kinds: Vec<String>,

    /// P7: number of successful L1 cycles (no regressions) required to unlock
    /// L2 auto-trigger for an agent. Default: 10.
    #[serde(default = "default_improve_l2_threshold")]
    pub l2_threshold: u64,

    /// P7: number of successful L2 cycles (no regressions) required to unlock
    /// L3 auto-approve for an agent. Default: 20.
    #[serde(default = "default_improve_l3_threshold")]
    pub l3_threshold: u64,

    /// P7: explicit per-agent opt-in allowlist for L3 auto-approve.
    /// Agents listed here may be auto-approved at L3 **if** they have also
    /// earned enough L2 track record. Never wildcarded.
    /// L3 never applies to agents with CodeExecution, AgentSpawn (broad),
    /// or sandbox-escape-adjacent capabilities regardless of this list.
    #[serde(default)]
    pub auto_approve_agents: Vec<String>,

    /// P7: maximum blast-radius score (0.0–1.0) for L3 auto-approval.
    /// Revisions scoring above this threshold require operator approval
    /// even if the agent is on the auto-approve list. Default: 0.3.
    #[serde(default = "default_improve_l3_blast_radius_threshold")]
    pub l3_blast_radius_threshold: f64,
}

fn default_improve_restrict_to_prompt_only() -> bool {
    true
}

fn default_improve_allow_capability_changes() -> bool {
    false
}

fn default_improve_capability_change_min_holdout() -> f64 {
    0.5
}

fn default_improve_high_blast_radius_capability_kinds() -> Vec<String> {
    vec![
        "SandboxFunctions".to_string(),
        "NetworkAccess".to_string(),
        "CodeExecution".to_string(),
        "ArtifactExecution".to_string(),
        "CredentialAccess".to_string(),
        "EmergencyStop".to_string(),
        "AgentRevision".to_string(),
        "SchedulerAccess".to_string(),
    ]
}

fn default_improve_l2_threshold() -> u64 {
    10
}

fn default_improve_l3_threshold() -> u64 {
    20
}

fn default_improve_l3_blast_radius_threshold() -> f64 {
    0.3
}

impl Default for ImproveConfig {
    fn default() -> Self {
        Self {
            restrict_to_prompt_only: default_improve_restrict_to_prompt_only(),
            allow_capability_changes: default_improve_allow_capability_changes(),
            capability_change_min_holdout: default_improve_capability_change_min_holdout(),
            high_blast_radius_capability_kinds:
                default_improve_high_blast_radius_capability_kinds(),
            l2_threshold: default_improve_l2_threshold(),
            l3_threshold: default_improve_l3_threshold(),
            auto_approve_agents: Vec::new(),
            l3_blast_radius_threshold: default_improve_l3_blast_radius_threshold(),
        }
    }
}

/// Auto-learning configuration: controls the default self-improvement pipeline.
///
/// When enabled, sessions automatically produce memories (via post-session digest)
/// and the memory curator runs periodically to distill cross-session knowledge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoLearningConfig {
    /// Master switch for the auto-learning pipeline. When false, post-session
    /// digest, quality signals, and periodic memory curation are all disabled
    /// regardless of their individual settings. Default: true.
    #[serde(default = "default_auto_learning_enabled")]
    pub enabled: bool,

    /// Emit a lightweight quality signal after each completed session, persisted
    /// as a Tier-2 memory tagged `source:quality_signal`. Default: true.
    #[serde(default = "default_auto_learning_enabled")]
    pub quality_signals: bool,

    /// Cron schedule for the periodic memory-curator run.
    /// Default: every 4 hours ("0 */4 * * *").
    #[serde(default = "default_curation_schedule")]
    pub curation_schedule: String,

    /// Score wake-time memory priming (context.rs) against the incoming task
    /// text (Jaccard token overlap) instead of pure recency. Default: true.
    #[serde(default = "default_true")]
    pub task_matched_recall: bool,
}

fn default_auto_learning_enabled() -> bool {
    true
}

fn default_curation_schedule() -> String {
    "0 */4 * * *".to_string()
}

impl Default for AutoLearningConfig {
    fn default() -> Self {
        Self {
            enabled: default_auto_learning_enabled(),
            quality_signals: default_auto_learning_enabled(),
            curation_schedule: default_curation_schedule(),
            task_matched_recall: default_true(),
        }
    }
}

/// Complexity profile that controls default behavior and visibility.
///
/// Profiles set sensible defaults for various config knobs; explicit overrides
/// in the config file always win. The profile is resolved once at config load
/// and does not change at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    /// Minimal setup, auto-learning ON, simplified TUI, generous session grants.
    Starter,
    /// Current behavior: all approvals require operator, full config surface.
    #[default]
    Standard,
    /// Full constitutional visibility, eval suite mandatory for promotion.
    Expert,
}

impl Profile {
    /// Whether safe tool invocations (e.g., read-only file ops, web_search) can
    /// skip explicit approval.  Starter auto-approves safe ops so new users are
    /// not overwhelmed.
    pub fn auto_approve_safe_tools(&self) -> bool {
        matches!(self, Self::Starter)
    }

    /// Whether the background scheduler should auto-start.
    pub fn background_scheduler_default(&self) -> bool {
        !matches!(self, Self::Starter)
    }

    /// Whether to display constitutional rule IDs alongside approval cards.
    pub fn show_rule_ids_in_approvals(&self) -> bool {
        matches!(self, Self::Expert)
    }

    /// Whether the TUI shows the full help text or a simplified version.
    pub fn simplified_help(&self) -> bool {
        matches!(self, Self::Starter)
    }

    /// Max Tier-2 memories to inject in session priming context.
    pub fn memory_priming_limit(&self) -> usize {
        match self {
            Self::Starter => 3,
            Self::Standard => 5,
            Self::Expert => 10,
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

/// Active constitution artifacts enforced by the gateway runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstitutionConfig {
    /// Markdown source file for the active constitution.
    #[serde(default = "default_constitution_source_path")]
    pub source_path: PathBuf,
    /// Lock file pinning digest/metadata for the active constitution.
    #[serde(default = "default_constitution_lock_path")]
    pub lock_path: PathBuf,
    /// Require a valid constitution lock signature at startup.
    #[serde(default = "default_require_constitution_signature")]
    pub require_signature: bool,
    /// Trusted signer registry (signer_id -> base64 32-byte Ed25519 public key).
    #[serde(default = "default_constitution_trusted_signers")]
    pub trusted_signers: HashMap<String, String>,
}

impl Default for ConstitutionConfig {
    fn default() -> Self {
        Self {
            source_path: default_constitution_source_path(),
            lock_path: default_constitution_lock_path(),
            require_signature: default_require_constitution_signature(),
            trusted_signers: default_constitution_trusted_signers(),
        }
    }
}

/// The single source of truth for the active ratified constitution version.
///
/// On a version bump, create `docs/constitution/versions/{VERSION}/`
/// (`constitution.md` + `RATIFY.md`), update this constant, and re-run
/// `docs/constitution/recompute_lock.py --version {VERSION}` — that one script
/// re-signs the lock **and** rewrites `docs/constitution/CURRENT`, so the
/// default paths, the `CURRENT` pointer, and the lock-inventory tests all stay
/// in sync from this one edit. The guard test
/// `current_file_matches_active_constitution_version` fails CI if the two drift.
pub const ACTIVE_CONSTITUTION_VERSION: &str = "2026.07.08";

pub fn default_constitution_source_path() -> PathBuf {
    PathBuf::from(format!(
        "docs/constitution/versions/{ACTIVE_CONSTITUTION_VERSION}/constitution.md"
    ))
}

pub fn default_constitution_lock_path() -> PathBuf {
    PathBuf::from(format!(
        "docs/constitution/versions/{ACTIVE_CONSTITUTION_VERSION}/gateway-constitution.lock.json"
    ))
}

fn default_require_constitution_signature() -> bool {
    true
}

fn default_constitution_trusted_signers() -> HashMap<String, String> {
    let mut out = HashMap::new();
    out.insert(
        "autonoetic:constitution:v1".to_string(),
        "lNxT1b/jWa6LqM2Thd7rW1IppvlH3rlEnAOPV81Igzk=".to_string(),
    );
    out
}

/// Compatibility policy for federated constitution checks (P-10.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FederationConstitutionMode {
    /// Require exact digest match.
    #[default]
    Exact,
    /// Accept exact match or configured known-compatible digests.
    KnownCompatible,
    /// Superset mode is reserved for rule/right table exchange.
    /// Until table exchange is enabled, behaves like known-compatible.
    Superset,
}

/// Federation constitution compatibility settings (P-10.9 scaffolding).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationConstitutionConfig {
    /// Compatibility mode.
    #[serde(default)]
    pub mode: FederationConstitutionMode,
    /// Additional digests accepted as compatible in non-exact modes.
    #[serde(default)]
    pub known_compatible_digests: Vec<String>,
    /// When true, peers that do not advertise a digest are accepted
    /// (legacy interop mode). Default: true.
    #[serde(default = "default_allow_missing_peer_constitution_digest")]
    pub allow_missing_peer_digest: bool,
}

impl Default for FederationConstitutionConfig {
    fn default() -> Self {
        Self {
            mode: FederationConstitutionMode::Exact,
            known_compatible_digests: Vec::new(),
            allow_missing_peer_digest: default_allow_missing_peer_constitution_digest(),
        }
    }
}

fn default_allow_missing_peer_constitution_digest() -> bool {
    true
}

/// Action taken by the stuck-task sweeper when a `Running` task has a stale
/// heartbeat / no progress and no completion evidence (session manifest,
/// digest, checkpoint, or implicit artifact).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StuckTaskNoEvidenceAction {
    /// Resolve the stuck task as `Failed` and finalize its session as failed.
    Fail,
    /// Preserve legacy behavior: resolve the stuck task as `Succeeded`.
    Succeed,
}

impl Default for StuckTaskNoEvidenceAction {
    fn default() -> Self {
        StuckTaskNoEvidenceAction::Fail
    }
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

    /// HTTP listen port for multi-channel ingress (`/api/content/*`, `/api/event/ingest`).
    /// Binds `0.0.0.0:{http_port}`. Set to `0` to disable the HTTP server (localhost-only JSON-RPC remains available).
    #[serde(default = "default_http_port")]
    pub http_port: u16,

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

    /// Constitution artifact paths that define the active constitutional law.
    #[serde(default)]
    pub constitution: ConstitutionConfig,

    /// Constitution compatibility policy for federated peers (P-10.9).
    #[serde(default)]
    pub federation_constitution: FederationConstitutionConfig,

    /// Maximum number of agent runtime executions allowed concurrently.
    #[serde(default = "default_max_concurrent_spawns")]
    pub max_concurrent_spawns: usize,

    /// Maximum number of pending executions admitted per target agent.
    /// This count includes the currently running execution for that agent.
    #[serde(default = "default_max_pending_spawns_per_agent")]
    pub max_pending_spawns_per_agent: usize,

    /// System-wide ceiling for spawn-chain depth (P-7.15).
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

    /// Default orchestrator agent ID for new sessions and workflows.
    /// When set, this agent is used as the lead/planner instead of `planner.default`.
    /// Can be overridden per-session via CLI or API. Default: "planner.default".
    #[serde(default = "default_default_orchestrator")]
    pub default_orchestrator: String,

    /// Code analysis configuration for agent.install validation.
    /// Controls how the gateway analyzes code for capabilities and security.
    #[serde(default)]
    pub code_analysis: CodeAnalysisConfig,

    /// Capability delta gating mode for `agent.revision.promote`.
    #[serde(default)]
    pub capability_delta_gate_mode: CapabilityDeltaGateMode,

    /// Promotion-completeness "cursor" (see docs/design/promotion-completeness-invariant.md).
    /// When `true` (default), a revision that declares **zero capabilities** may be
    /// promoted directly — it cannot invoke any privileged tool, so its blast radius
    /// is bounded by runtime capability enforcement regardless of provenance. Set to
    /// `false` to require the full audit/approval gate even for zero-capability agents.
    /// Capability-bearing revisions are always gated (fail-closed) regardless of this flag.
    #[serde(default = "default_true")]
    pub allow_zero_capability_direct_promote: bool,

    /// Promotion-completeness cursor for **first admission of a brand-new agent**
    /// (no outgoing revision). When `true` (default), promoting a new
    /// capability-bearing agent requires operator approval — its whole capability
    /// set is "new", so it is treated as maximal broadening (R++2). Set to `false`
    /// to let a fully-audited new agent self-promote (autonomous self-evolution):
    /// the completeness gate (auditor/evaluator pass, distinct identities,
    /// reviewable artifact) still applies and is always fail-closed; only the
    /// human-approval requirement for *being new* is lifted. Re-promotion of an
    /// already-admitted agent is unaffected either way (gated only on broadening).
    #[serde(default = "default_true")]
    pub require_operator_approval_for_new_agents: bool,

    /// **Deprecated (#578):** ignored — smoke-test requirement is derived from the
    /// candidate revision's declared capabilities. Retained for YAML compatibility.
    #[serde(default)]
    pub agent_install_smoke_test: AgentInstallSmokeTestMode,

    /// Optional per-session budgets (LLM rounds, tools, tokens, wall clock).
    #[serde(default)]
    pub session_budget: SessionBudgetConfig,

    /// Tree-wide budgets aggregated across all descendants of a root session (P-6.21).
    /// Applies in addition to per-session limits; the tighter bound wins.
    #[serde(default)]
    pub root_session_budget: RootSessionBudgetConfig,

    /// Maximum seconds a workflow task may remain in `AwaitingApproval` before it is
    /// automatically marked `Stale`. Set to 0 to disable (not recommended for production).
    /// A `Stale` task preserves its checkpoint (the approval can still be resolved later)
    /// and counts as terminal for workflow joins, but is visibly not `Failed`.
    /// Default: 600 (10 minutes).
    #[serde(default = "default_approval_timeout_secs")]
    pub approval_timeout_secs: u64,

    /// Maximum seconds a **standalone** (non-workflow) approval may remain pending
    /// before it is flagged as stale. Unlike workflow tasks, a stale standalone
    /// approval is NOT automatically cancelled — it is surfaced as stale in
    /// `operator.pending` so operators can resolve it. Set to 0 to disable.
    /// Default: 86400 (24 hours).
    #[serde(default = "default_standalone_approval_timeout_secs")]
    pub standalone_approval_timeout_secs: u64,

    /// Maximum seconds a user interaction may remain pending before it is
    /// automatically marked `expired`. Set to 0 to disable.
    /// Default: 86400 (24 hours).
    #[serde(default = "default_interaction_timeout_secs")]
    pub interaction_timeout_secs: u64,

    /// Maximum seconds an escalation may remain pending before it is
    /// automatically marked `stale`. Set to 0 to disable.
    /// Default: 86400 (24 hours).
    #[serde(default = "default_escalation_timeout_secs")]
    pub escalation_timeout_secs: u64,

    /// Maximum seconds a plan frame may remain in `awaiting_approval` before
    /// it is automatically marked `stale`. Set to 0 to disable.
    /// Default: 86400 (24 hours).
    #[serde(default = "default_plan_frame_timeout_secs")]
    pub plan_frame_timeout_secs: u64,

    /// Maximum number of concurrent pending approvals per root_session_id (P-7.17).
    /// When a new approval request would push the count above this cap, the insert is
    /// rejected with `approval_flood`. Set to 0 to disable (not recommended).
    /// Default: 50.
    #[serde(default = "default_max_pending_approvals_per_root")]
    pub max_pending_approvals_per_root: usize,

    /// Maximum number of concurrent pending escalations per root_session_id.
    /// When a new escalation would push the count above this cap, the insert is
    /// rejected with `escalation_flood`. Set to 0 to disable.
    /// Default: 50.
    #[serde(default = "default_max_pending_escalations_per_root")]
    pub max_pending_escalations_per_root: usize,

    /// Maximum number of concurrent un-adjudicated anomaly flags
    /// (`pending`/`under_review`) per reporter agent — the Ri-0.18 spam
    /// triage bound (#770). `anomaly_flag` intake is capability-free, so a
    /// prompt-injected reporter could otherwise flood the review queue. A
    /// filing that would push the count above this cap is rejected loudly
    /// with `anomaly_flag_flood` (never silently dropped); terminal
    /// adjudications (confirmed/dismissed/deferred) free capacity.
    /// Set to 0 to disable (not recommended). Default: 50.
    #[serde(default = "default_max_pending_anomaly_flags_per_reporter")]
    pub max_pending_anomaly_flags_per_reporter: usize,

    /// Default TTL in seconds for auto-generated session approval grants.
    /// When an approval is resolved and a grant is auto-inserted without an
    /// explicit `--ttl`/`--until` override, `expires_at` is set to
    /// `now + default_grant_ttl_secs`. Set to 0 to disable auto-expiry
    /// (grants live until revoked or emergency stop). Default: 86400 (24h).
    #[serde(default = "default_grant_ttl_secs")]
    pub default_grant_ttl_secs: u64,

    /// Number of sandbox-escape indicators per session that triggers P-7.18
    /// degraded mode. Set to 0 to disable. Default: 5.
    #[serde(default = "default_escape_attempt_degrade_threshold")]
    pub escape_attempt_degrade_threshold: usize,

    /// Number of sandbox-escape indicators per session that triggers emergency
    /// stop. Set to 0 to disable. Default: 20.
    #[serde(default = "default_escape_attempt_emergency_threshold")]
    pub escape_attempt_emergency_threshold: usize,

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

    /// What the stuck-task sweeper should do when a `Running` task has no completion
    /// evidence and no fresh heartbeat. `fail` (default) resolves the task as `Failed`
    /// and emits a `task.stuck` anomaly event. `succeed` preserves legacy behavior and
    /// force-completes it as `Succeeded`.
    #[serde(default)]
    pub stuck_task_no_evidence_action: StuckTaskNoEvidenceAction,

    /// Evidence mode configuration.
    /// Controls how much tool/LLM execution data is saved to evidence files for debugging.
    /// "full": all tool results and LLM completions (default for development)
    /// "errors": only failures, approval gates, non-zero exit codes (production recommended)
    /// "off": no evidence files (causal_events DB still captures everything)
    #[serde(default)]
    pub evidence_mode: String,

    /// Live/session structured report files (`session_overview.md`, `session_report.*`).
    #[serde(default)]
    pub session_report: SessionReportConfig,

    /// Optional post-session digest (narrative + extracted memories). Off by default — enable in config.
    #[serde(default)]
    pub digest_agent: DigestAgentConfig,

    /// Outcome grader: optional LLM-graded `Completion` verdict attached
    /// to each `SessionOutcome` row. The auto-populated metrics
    /// (cost/tokens/turns/wall) are written regardless; this gates only
    /// the grade. Off by default.
    #[serde(default)]
    pub outcome_grader: OutcomeGraderConfig,

    /// Self-improvement loop guardrails. Default posture:
    /// `restrict_to_prompt_only = true` — the A/B replay tool will
    /// refuse to compare two revisions whose declared capability or
    /// tool-tier surfaces differ. See `ImproveConfig`.
    #[serde(default)]
    pub improve: ImproveConfig,

    /// Data retention settings (days). 0 = retain forever.
    #[serde(default)]
    pub retention: RetentionConfig,

    /// Cognitive Capsule export/import settings (signing trust, size caps,
    /// default mode). See `docs/cognitive-capsule.md`.
    #[serde(default)]
    pub capsule: CapsuleConfig,

    /// Resource reclamation (garbage collection) settings.
    /// Idempotent sweep for content blobs, old revisions, expired memories,
    /// orphaned sessions, and stale scheduled jobs.
    #[serde(default)]
    pub reclamation: ReclamationConfig,

    /// Response validation gate configuration.
    /// When enabled, the gateway validates agent outputs against declared constraints
    /// in agent metadata before returning SpawnResult to the caller.
    #[serde(default)]
    pub response_validation: ResponseValidationConfig,

    /// Optional validation-waiver workflow (#333).
    /// Disabled by default; enable to let operators skip advisory validations
    /// through a TUI picklist or `/waive` command.
    #[serde(default)]
    pub validation_waivers: ValidationWaiversConfig,

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

    /// In-session divergence monitor (Sentinel) configuration. Observes
    /// loop/failure/repetition/stall/error/context signals and emits
    /// `divergence.*` causal events on level transitions. Observational
    /// only — does not modify session behavior.
    #[serde(default)]
    pub trajectory: TrajectoryConfig,

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

    /// Operator activity feed settings (rate limiting). See the operator
    /// activity feed design (`docs/design/operator-activity-feed-plan.md`).
    #[serde(default)]
    pub operator_activity: OperatorActivityConfig,

    /// Symmetric decider-obligation enforcement (#359 §O / #395).
    #[serde(default)]
    pub decider_obligations: DeciderObligationsConfig,

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

    /// Default blocking timeout for `workflow.wait` when the caller omits
    /// `timeout_secs`. The tool blocks until all watched task_ids reach a
    /// terminal state or this deadline elapses. Set to 0 to restore the
    /// legacy immediate-return behaviour. Default: 30.
    #[serde(default = "default_workflow_wait_secs")]
    pub default_workflow_wait_secs: u64,

    /// Server-side total wall-clock budget for a single `workflow.wait` call
    /// (issue #702). When a wait chunk (`timeout_secs`, default
    /// `default_workflow_wait_secs`) elapses with tasks still running, the
    /// gateway re-issues the wait internally — without returning to the LLM —
    /// until all watched task IDs reach a terminal state or this total budget
    /// is exhausted. This eliminates the expensive `wait → timeout → full LLM
    /// round → wait` churn where the model re-reads the whole context only to
    /// re-issue the same wait. Callers may lower it per-call via `max_wait_secs`.
    /// Set equal to `default_workflow_wait_secs` (or 0) to disable
    /// auto-extension. Default: 300 (5 minutes).
    #[serde(default = "default_workflow_wait_max_total_secs")]
    pub workflow_wait_max_total_secs: u64,

    #[serde(default)]
    pub hooks: Vec<crate::hooks::HookConfig>,

    /// Scheduled jobs (cron) configuration.
    #[serde(default)]
    pub scheduled_jobs: ScheduledJobsConfig,

    /// Promotion safety governor (issue #25). Per-alias velocity, flapping,
    /// and eval-regression checks enforced at `agent.revision.promote`.
    #[serde(default)]
    pub promotion_governor: PromotionGovernorConfig,

    /// Fast scheduler sidecar configuration. Disabled by default.
    #[serde(default)]
    pub fast_scheduler: FastSchedulerConfig,

    /// System agents: declared background agents that the gateway reconciles on startup.
    #[serde(default)]
    pub system_agents: Vec<SystemAgentEntry>,

    /// When true (default), JSON-RPC `interaction.answer` / `interaction.resolve_and_answer`
    /// persist answers and orchestrate workflow task or session resume (gateway-owned path).
    #[serde(default = "default_interaction_answer_orchestration")]
    pub interaction_answer_orchestration: bool,

    /// Allow sessions to start even when runtime.lock drift is detected (P-8.12).
    /// When true, drift is logged as a causal event but does not block session start.
    /// Default: false (drift is fatal).
    #[serde(default)]
    pub allow_runtime_lock_drift: bool,

    /// Auto-approve a PlanFrame the moment it is proposed, instead of waiting for
    /// an authority to approve it. A convenience for local/dev and autonomous
    /// runs where no operator is in the loop. Default: false — plans await an
    /// authority (separation of powers: agents propose, an authority disposes).
    /// The approval is recorded under `plan_auto_approver` so the causal chain
    /// shows it was an automatic decision, not a human/agent authority.
    #[serde(default)]
    pub plan_auto_approve: bool,

    /// Identity recorded as the approver when `plan_auto_approve` is enabled.
    /// Default: "auto-approve" — documents in the audit trail that approval was
    /// automatic rather than a deliberate authority decision.
    #[serde(default = "default_plan_auto_approver")]
    pub plan_auto_approver: String,

    /// When true, allow revision creation to proceed without a gateway signature
    /// when the gateway identity key is unavailable (e.g. first boot on a read-only
    /// filesystem, permission errors). In normal operation the gateway auto-signs every
    /// revision — this flag is only an escape hatch for environments where the key
    /// cannot be loaded or generated. Default: false.
    #[serde(default)]
    pub trust_unsigned_bundles: bool,

    /// Multiplier applied to approval dwell times (P-2.24). Set to 0 to
    /// disable dwell-time enforcement (for tests). Default: 1.0.
    #[serde(default = "default_approval_dwell_multiplier")]
    pub approval_dwell_multiplier: f64,

    /// Security sentinel configuration.
    #[serde(default)]
    pub sentinel: SentinelConfig,

    /// Protected agents configuration (issue #21).
    /// Agents listed here require a passed eval run before programmatic
    /// promotion is allowed. The sentinel gate (if enabled) still fires
    /// independently for all promotions.
    #[serde(default)]
    pub protected_agents: ProtectedAgentsConfig,

    /// Complexity profile: starter / standard / expert.
    /// Controls default behavior and visibility. Explicit config overrides always win.
    #[serde(default)]
    pub profile: Profile,

    /// Auto-learning pipeline configuration.
    /// Controls post-session digest, quality signals, and periodic memory curation.
    #[serde(default)]
    pub auto_learning: AutoLearningConfig,

    /// Path to a persona file (Markdown) injected into every agent's system
    /// prompt. Defines cross-agent user context, communication preferences,
    /// or domain background. Relative paths resolve from the config directory.
    /// Default: `persona.md` next to the config file (used only if the file exists).
    #[serde(default)]
    pub persona_path: Option<PathBuf>,

    /// Wiki proposal governance: auto-expiry, quality heuristics, duplicate detection.
    #[serde(default)]
    pub wiki_proposal: WikiProposalConfig,

    /// Session Room: timeline altitude tuning surface (role floors).
    /// Config is parsed and validated; runtime plumbing to apply it during
    /// altitude computation is tracked separately.
    #[serde(default)]
    pub session_room: SessionRoomConfig,
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

/// Promotion safety governor (issue #25).
///
/// Three gate-level checks applied at `agent.revision.promote` *before*
/// `atomic_promote`:
/// - **velocity**: max promotions per alias per window
/// - **flapping**: re-promoting a recently-promoted revision_id
/// - **eval-regression**: consecutive monotonic increases in non-info finding
///   counts across recent verdicts
///
/// All three are bypassable via `force: true` + `force_reason` on the promote
/// call (emits a `governor.override` causal event for the audit trail).
/// Disabled by default for backwards compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromotionGovernorConfig {
    /// Enable governor checks. Default: false (opt-in).
    #[serde(default = "default_promotion_governor_enabled")]
    pub enabled: bool,

    /// Velocity-check window in hours. Default: 24.
    #[serde(default = "default_promotion_governor_velocity_window_hours")]
    pub velocity_window_hours: u64,

    /// Maximum number of `Promote` entries per alias inside the velocity
    /// window. Default: 3.
    #[serde(default = "default_promotion_governor_max_promotions_per_window")]
    pub max_promotions_per_window: usize,

    /// How many most-recent promotions to scan when looking for the candidate
    /// revision_id (flapping signal: re-promoting a revision already seen
    /// recently). Default: 4.
    #[serde(default = "default_promotion_governor_flapping_lookback")]
    pub flapping_lookback: usize,

    /// How many adjacent finding-count comparisons must be strictly increasing
    /// for the eval-regression halt to fire. Default: 3 (i.e. counts c0 < c1
    /// < c2 < c3 → 3 increases → halt).
    #[serde(default = "default_promotion_governor_eval_regression_streak")]
    pub eval_regression_streak: usize,

    /// Maximum number of recent promotions to scan for the eval-regression
    /// streak. Default: 6.
    #[serde(default = "default_promotion_governor_eval_regression_lookback")]
    pub eval_regression_lookback: usize,

    /// Maximum number of rejected promotion attempts for the same
    /// `(alias, content_digest)` before further attempts are blocked until an
    /// operator ack resets the counter (issue #720). Default: 3.
    #[serde(default = "default_promotion_governor_max_promotion_attempts_per_revision")]
    pub max_promotion_attempts_per_revision: usize,
}

impl Default for PromotionGovernorConfig {
    fn default() -> Self {
        Self {
            enabled: default_promotion_governor_enabled(),
            velocity_window_hours: default_promotion_governor_velocity_window_hours(),
            max_promotions_per_window: default_promotion_governor_max_promotions_per_window(),
            flapping_lookback: default_promotion_governor_flapping_lookback(),
            eval_regression_streak: default_promotion_governor_eval_regression_streak(),
            eval_regression_lookback: default_promotion_governor_eval_regression_lookback(),
            max_promotion_attempts_per_revision:
                default_promotion_governor_max_promotion_attempts_per_revision(),
        }
    }
}

/// Fast scheduler sidecar configuration.
///
/// Runs a low-latency parallel loop beside the canonical background
/// scheduler, targeting interval-style jobs (`every N seconds`). The DB
/// `claim_and_advance_due_job` call remains the source of truth, so the
/// two loops cannot double-dispatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FastSchedulerConfig {
    /// Enable the fast scheduler sidecar. Default: false.
    #[serde(default = "default_fast_scheduler_enabled")]
    pub enabled: bool,

    /// Tick interval in milliseconds. Default: 200ms.
    #[serde(default = "default_fast_scheduler_tick_millis")]
    pub tick_millis: u64,

    /// Maximum number of candidate jobs admitted per tick. Default: 64.
    #[serde(default = "default_fast_scheduler_max_due_per_tick")]
    pub max_due_per_tick: usize,
}

impl Default for FastSchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: default_fast_scheduler_enabled(),
            tick_millis: default_fast_scheduler_tick_millis(),
            max_due_per_tick: default_fast_scheduler_max_due_per_tick(),
        }
    }
}

fn default_promotion_governor_enabled() -> bool {
    false
}

fn default_promotion_governor_velocity_window_hours() -> u64 {
    24
}

fn default_promotion_governor_max_promotions_per_window() -> usize {
    3
}

fn default_promotion_governor_flapping_lookback() -> usize {
    4
}

fn default_promotion_governor_eval_regression_streak() -> usize {
    3
}

fn default_promotion_governor_eval_regression_lookback() -> usize {
    6
}

fn default_promotion_governor_max_promotion_attempts_per_revision() -> usize {
    3
}

fn default_fast_scheduler_enabled() -> bool {
    false
}

fn default_fast_scheduler_tick_millis() -> u64 {
    200
}

fn default_fast_scheduler_max_due_per_tick() -> usize {
    64
}

/// Security sentinel configuration.
///
/// The sentinel runs deterministic and heuristic checks against the gateway's
/// local SQLite store. It has two operating modes:
///
/// - **Scheduled sweeps**: registered as internal cron jobs at gateway startup.
/// - **Promotion gate**: a scoped sweep fires synchronously before
///   `atomic_promote` is called; promotion is blocked until it completes or
///   times out (fail-closed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentinelConfig {
    /// Enable the security sentinel. Default: true.
    #[serde(default = "default_sentinel_enabled")]
    pub enabled: bool,

    /// Cron schedule for a full (all-history) sentinel sweep. Default: daily at 03:00 UTC.
    #[serde(default = "default_sentinel_full_sweep_schedule")]
    pub full_sweep_schedule: String,

    /// Cron schedule for an incremental sweep (last 24 h). Default: every 6 hours.
    #[serde(default = "default_sentinel_incremental_sweep_schedule")]
    pub incremental_sweep_schedule: String,

    /// Block agent promotion when the pre-promotion sentinel sweep finds any
    /// `critical` severity findings. Default: true (fail-closed).
    #[serde(default = "default_sentinel_promotion_gate_enabled")]
    pub promotion_gate_enabled: bool,

    /// Maximum seconds the promotion gate waits for the sentinel sweep to complete.
    /// If the sweep exceeds this limit, promotion is blocked (fail-closed). Default: 30.
    #[serde(default = "default_sentinel_promotion_gate_timeout_secs")]
    pub promotion_gate_timeout_secs: u64,

    /// Sentinel revision ID embedded in findings produced by the live sentinel.
    /// Update this when the sentinel logic changes to track which version flagged
    /// a finding. Default: "sentinel.current".
    #[serde(default = "default_sentinel_revision_id")]
    pub sentinel_revision_id: String,

    /// Sentinel revision ID for the frozen baseline. Default: "sentinel.baseline.frozen".
    #[serde(default = "default_sentinel_baseline_revision_id")]
    pub baseline_revision_id: String,
}

impl Default for SentinelConfig {
    fn default() -> Self {
        Self {
            enabled: default_sentinel_enabled(),
            full_sweep_schedule: default_sentinel_full_sweep_schedule(),
            incremental_sweep_schedule: default_sentinel_incremental_sweep_schedule(),
            promotion_gate_enabled: default_sentinel_promotion_gate_enabled(),
            promotion_gate_timeout_secs: default_sentinel_promotion_gate_timeout_secs(),
            sentinel_revision_id: default_sentinel_revision_id(),
            baseline_revision_id: default_sentinel_baseline_revision_id(),
        }
    }
}

fn default_sentinel_enabled() -> bool { true }
fn default_sentinel_full_sweep_schedule() -> String { "0 3 * * *".to_string() }
fn default_sentinel_incremental_sweep_schedule() -> String { "0 */6 * * *".to_string() }
fn default_sentinel_promotion_gate_enabled() -> bool { true }
fn default_sentinel_promotion_gate_timeout_secs() -> u64 { 30 }
fn default_sentinel_revision_id() -> String { "sentinel.current".to_string() }
fn default_sentinel_baseline_revision_id() -> String { "sentinel.baseline.frozen".to_string() }

/// Protected agents configuration (issue #21).
///
/// Protected agents are critical agents whose promotion is mechanically gated
/// beyond the normal artifact + capability-delta gates. This closes the
/// recursive-trust problem: a regressed agent-factory cannot silently
/// replace itself without passing an independent check.
///
/// The gate enforces that:
/// 1. A passed eval run (`required_eval_run_id`) must be provided.
/// 2. The eval run must target the exact revision being promoted.
/// 3. The sentinel pre-promotion gate must pass (if enabled).
///
/// Operators can still promote by hand via the CLI, but the programmatic
/// path (agent-driven `agent_revision_promote`) is mechanically blocked
/// for protected agents unless the eval evidence is presented.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtectedAgentsConfig {
    /// List of agent IDs that are protected. Default: empty (no extra gate).
    #[serde(default)]
    pub agents: Vec<String>,

    /// Whether the protected-agent gate is enabled. Default: true.
    /// Set to false to disable in development.
    #[serde(default = "default_protected_agents_enabled")]
    pub enabled: bool,
}

impl Default for ProtectedAgentsConfig {
    fn default() -> Self {
        Self {
            agents: Vec::new(),
            enabled: default_protected_agents_enabled(),
        }
    }
}

fn default_protected_agents_enabled() -> bool {
    true
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatConfig {
    /// Allow inline approval of pending requests from the chat TUI (Ctrl+A).
    /// Defaults to true for interactive local use; set `false` to require
    /// `autonoetic gateway approvals …` outside the chat pane.
    #[serde(default = "default_chat_inline_approvals")]
    pub inline_approvals: bool,
}

fn default_chat_inline_approvals() -> bool {
    true
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            inline_approvals: default_chat_inline_approvals(),
        }
    }
}

/// Symmetric decider obligations (#359 §O / #395). When enabled, the gateway
/// refuses a BLOCKING-tier gate decision that carries no motivation — a
/// rejection, or a principal's approval of an elevated-authority or
/// external/irreversible action. Mechanical resolutions (no principal) and
/// reversible operator-level approvals are exempt. The gateway checks only the
/// *presence* of a reason, never its quality (Lawful Executor).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeciderObligationsConfig {
    /// Require a motivation for BLOCKING-tier decisions. Default: true.
    #[serde(default = "default_decider_obligations_enabled")]
    pub enabled: bool,

    /// Adjudication SLA in seconds (#771 D.1): a constitutional proposal
    /// (O-6) or anomaly flag (O-7) still un-adjudicated past this deadline is
    /// flagged as an SLA breach (does not change status). `0` disables the
    /// check. Default: 7 days.
    #[serde(default = "default_adjudication_sla_secs")]
    pub adjudication_sla_secs: u64,
}

impl Default for DeciderObligationsConfig {
    fn default() -> Self {
        Self {
            enabled: default_decider_obligations_enabled(),
            adjudication_sla_secs: default_adjudication_sla_secs(),
        }
    }
}

fn default_decider_obligations_enabled() -> bool {
    true
}

fn default_adjudication_sla_secs() -> u64 {
    604800
}

/// Configuration for the operator activity feed (Phase 4 hardening).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorActivityConfig {
    /// Maximum operator activity rows persisted per root session per rolling
    /// 60-second window. When the cap is reached the gateway drops further
    /// rows in that window but emits a single `rate_limited` notice so the
    /// suppression is always visible (never silent). `0` disables the limit.
    /// Default: 120.
    #[serde(default = "default_operator_activity_rate_limit_per_min")]
    pub rate_limit_per_min: u32,
    /// Maximum age (in days) of operator activity rows before automatic
    /// pruning. Default: 90.
    #[serde(default = "default_operator_activity_retention_days")]
    pub retention_days: i64,
}

impl Default for OperatorActivityConfig {
    fn default() -> Self {
        Self {
            rate_limit_per_min: default_operator_activity_rate_limit_per_min(),
            retention_days: default_operator_activity_retention_days(),
        }
    }
}

fn default_operator_activity_rate_limit_per_min() -> u32 {
    120
}

fn default_operator_activity_retention_days() -> i64 {
    90
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

/// Configuration for Cognitive Capsule export/import.
///
/// Capsules are revision-pinned, optionally signed portable snapshots of
/// agents (see `docs/cognitive-capsule.md`). This
/// section controls signing-trust, size limits, and default export mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapsuleConfig {
    /// Trusted signer registry for capsule import verification.
    /// Keys are signer IDs (e.g. `gateway:<fingerprint>` or `user:<id>`);
    /// values are base64-encoded 32-byte Ed25519 public keys.
    #[serde(default)]
    pub trusted_signers: HashMap<String, String>,

    /// Default export mode when not specified on the CLI / tool call.
    /// One of `"thin"`, `"hermetic"`, `"replay"`, `"headless"`.
    #[serde(default = "default_capsule_mode")]
    pub default_mode: String,

    /// Maximum capsule archive size in bytes; importers refuse larger
    /// archives. Default: 2 GiB.
    #[serde(default = "default_capsule_max_size_bytes")]
    pub max_capsule_size_bytes: u64,

    /// Whether to auto-sign exported capsules with the gateway's
    /// Ed25519 signing key (the same key used for agent revisions).
    /// Default: true.
    #[serde(default = "default_capsule_auto_sign")]
    pub auto_sign: bool,

    /// Whether `--include-memory` is implied for exports that do not
    /// explicitly set the flag. Default: false (opt-in).
    #[serde(default)]
    pub include_memory_by_default: bool,
}

impl Default for CapsuleConfig {
    fn default() -> Self {
        Self {
            trusted_signers: HashMap::new(),
            default_mode: default_capsule_mode(),
            max_capsule_size_bytes: default_capsule_max_size_bytes(),
            auto_sign: default_capsule_auto_sign(),
            include_memory_by_default: false,
        }
    }
}

fn default_capsule_mode() -> String {
    "thin".to_string()
}

fn default_capsule_max_size_bytes() -> u64 {
    2 * 1024 * 1024 * 1024
}

fn default_capsule_auto_sign() -> bool {
    true
}

/// Configuration for resource reclamation (garbage collection).
///
/// The reclamation sweep runs on a configurable schedule and reclaims:
/// - Content blobs with zero remaining name references
/// - Memories past their `expires_at` deadline
/// - Archived agent revisions older than N days
/// - Orphaned sessions not resumed within N days
/// - Stale scheduled jobs whose root session has been closed for > N days
///
/// The sweep is idempotent, conservative (only deletes provably unreferenced data),
/// and every deletion is recorded in the causal event chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReclamationConfig {
    /// Enable the reclamation sweep. Default: false (opt-in).
    #[serde(default)]
    pub enabled: bool,

    /// Minimum interval (seconds) between sweeps. Default: 86400 (24h).
    #[serde(default = "default_reclamation_interval_secs")]
    pub min_interval_secs: u64,

    /// Delete content blobs with zero remaining name references after N days.
    /// 0 = skip this category. Default: 90.
    #[serde(default = "default_reclamation_content_blob_days")]
    pub content_blob_max_age_days: u64,

    /// Delete memories whose `expires_at` has passed.
    /// Always runs if `expires_at` is set on memories (column-level, not config-level).
    /// This is a safety switch: 0 = skip. Default: 0 (skip).
    #[serde(default)]
    pub expired_memory_retention_days: u64,

    /// Delete archived agent revisions older than N days. 0 = skip. Default: 180.
    #[serde(default = "default_reclamation_archived_revision_days")]
    pub archived_revision_max_age_days: u64,

    /// Mark sessions as closed if they are still `active` and their last activity
    /// is older than N days. 0 = skip. Default: 30.
    #[serde(default = "default_reclamation_orphaned_session_days")]
    pub orphaned_session_max_age_days: u64,

    /// Cancel `active` scheduled jobs whose root session has been closed for
    /// more than N days. 0 = skip. Default: 60.
    #[serde(default = "default_reclamation_stale_job_days")]
    pub stale_job_max_age_days: u64,
}

impl Default for ReclamationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_interval_secs: 86400,
            content_blob_max_age_days: 90,
            expired_memory_retention_days: 0,
            archived_revision_max_age_days: 180,
            orphaned_session_max_age_days: 30,
            stale_job_max_age_days: 60,
        }
    }
}

fn default_reclamation_interval_secs() -> u64 { 86400 }
fn default_reclamation_content_blob_days() -> u64 { 90 }
fn default_reclamation_archived_revision_days() -> u64 { 180 }
fn default_reclamation_orphaned_session_days() -> u64 { 30 }
fn default_reclamation_stale_job_days() -> u64 { 60 }

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

    /// Global hard ceiling for auto-repair attempts, regardless of agent request.
    #[serde(default = "default_response_validation_max_repair_attempts_ceiling")]
    pub max_repair_attempts_ceiling: u32,
}

impl Default for ResponseValidationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            repair_enabled: false,
            max_repair_attempts_ceiling: default_response_validation_max_repair_attempts_ceiling(),
        }
    }
}

fn default_response_validation_max_repair_attempts_ceiling() -> u32 {
    2
}

/// Configuration for the optional validation-waiver workflow (#333).
///
/// Validation waivers let operators skip advisory artifact validations
/// (unit tests, style review, etc.) while recording the skip as audit
/// provenance. Mechanical safety gates and security reviews can never be
/// waived. The feature is opt-in and defaults to disabled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationWaiversConfig {
    /// Enable the operator-facing validation waiver workflow.
    /// When false (default), `/waive` is not offered in the TUI and the
    /// backend waiver tools are not surfaced to operators, but existing
    /// waivers remain queryable.
    #[serde(default)]
    pub enabled: bool,

    /// Automatically propose the waiver picklist after a successful
    /// `workbench reconcile`. When false (default), the operator must
    /// explicitly trigger `/waive`.
    #[serde(default)]
    pub auto_propose_after_reconcile: bool,
}

impl Default for ValidationWaiversConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_propose_after_reconcile: false,
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

    /// Operator-level permission for sandbox recording mode (RFC scope
    /// 5.1 / 5.3 — sealed-network sandbox).
    ///
    /// When `false` (default), any session whose manifest declares
    /// `sandbox_network: recording` refuses to start. The flag exists so
    /// recording — which captures live network responses as fixtures —
    /// is never silently enabled by an agent's manifest declaration
    /// alone. Operators must opt the gateway in explicitly.
    ///
    /// Until 5.3 ships, this flag has no effect beyond the refuse-boot
    /// guard.
    #[serde(default)]
    pub allow_recording: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            share_net: false,
            dev_mode: default_sandbox_dev_mode(),
            allow_recording: false,
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

    /// Per-tool caps on how many times a successful tool result may reset
    /// `current_loops` in a session. After the budget for a tool name is exhausted,
    /// further successes from that tool no longer reset the loop counter. Prevents
    /// alternating `knowledge_store`/`knowledge_search`
    /// fingerprints from resetting progress indefinitely.
    #[serde(default = "default_progress_budget_tools")]
    pub progress_budget_tools: HashMap<String, u32>,

    /// Rotating-polling detector window (issue #287). The detector tracks
    /// the last N successful-tool-call fingerprints; trips when the window
    /// is full and has only `rotation_distinct_floor` or fewer distinct
    /// (tool, args) values. Catches agents that cycle through a small set
    /// of read-only tools (e.g. `workflow.wait → workflow.state →
    /// content.read → artifact.inspect → agent.exists`) without making
    /// semantic progress. Set to 0 to disable.
    #[serde(default = "default_rotation_window_size")]
    pub rotation_window_size: usize,

    /// Trip threshold for the rotating-polling detector. When the window
    /// is full and the distinct fingerprint count is <= this value, the
    /// guard trips. With the default `rotation_window_size = 16`, a floor
    /// of 6 means any rotation with 6 or fewer unique calls in the last
    /// 16 trips; healthy varied work with 7+ unique calls passes.
    #[serde(default = "default_rotation_distinct_floor")]
    pub rotation_distinct_floor: usize,

    /// Fast-path trip for read-only roster polling. When a read-only roster
    /// tool (`agent_list`, `agent_inspect`, `agent_discover`) is called this
    /// many times consecutively with the same normalized arguments, the guard
    /// trips with a corrective `RedundantRosterPolling` reason — without
    /// waiting for the generic `rotation_window_size` (16) window to fill.
    /// These directory reads are idempotent: re-listing never surfaces new
    /// data, so a tight repeat is always a stuck spawn, not progress. The
    /// trip message tells the agent to spawn directly with a free-text
    /// message or end the turn. Set to 0 to disable the fast path (the
    /// generic rotating-polling detector still applies).
    #[serde(default = "default_roster_repeat_floor")]
    pub roster_repeat_floor: u32,

    /// Max consecutive LLM transport/endpoint failures before tripping. When
    /// the model API is flapping (timeouts, connection refused, 5xx), the
    /// guard trips to prevent expensive retry spirals. A successful LLM call
    /// resets the counter to 0.
    #[serde(default = "default_max_llm_failures")]
    pub max_llm_failures: u32,

    /// Loop-counter penalty added to `current_loops` on each child task failure
    /// (issue #704). A queued `agent_spawn` returns `ok: true` and resets the
    /// no-progress counter, but a child that later fails means that spawn made
    /// no net progress — so each `any_failed` result advances `current_loops`
    /// by this amount (it does NOT reset it). Combined with read-only tools no
    /// longer resetting progress (#701), a spawn→probe→spawn death spiral now
    /// reaches `max_loops_without_progress`. Set to 0 to disable (legacy behavior).
    #[serde(default = "default_child_failure_loop_penalty")]
    pub child_failure_loop_penalty: u32,

    /// Recurring-error detector window (issue #703). The guard fingerprints each
    /// error tool-result (volatile ids/timestamps/numbers stripped) and tracks
    /// the last N in a sliding window. Set to 0 to disable the detector.
    #[serde(default = "default_recurring_error_window")]
    pub recurring_error_window: usize,

    /// Recurring-error trip threshold (issue #703). When the same normalized
    /// error fingerprint has surfaced from at least this many *distinct* tool
    /// names within `recurring_error_window`, the guard trips with
    /// `RecurringUnrecoverableError`. Catches an agent rotating through
    /// different tools that all hit one unrecoverable root cause — a pattern the
    /// per-tool failure budgets miss.
    #[serde(default = "default_recurring_error_distinct_tools")]
    pub recurring_error_distinct_tools: usize,

    /// Repeated-irrecoverable-rejection trip threshold (issue #718). Permission
    /// / quota / sandbox-unavailable rejections are deliberately excluded from
    /// the per-tool failure budget (`max_tool_failures`) — the agent cannot fix
    /// them by retrying with different arguments, so the first occurrences are
    /// free (a gateway-side block is not divergence; the agent legitimately
    /// ends its turn to wait for an operator). But re-issuing the *same* call
    /// and getting the *same* deterministic rejection is a no-progress loop
    /// (P-7.7): the agent re-asked a question the gateway already answered. When
    /// the same `(tool, normalized-error)` rejection recurs this many times the
    /// guard trips with `RepeatedIrrecoverableRejection`. Distinct rejections
    /// never accumulate together (fixing one gate and hitting the next is
    /// progress). Set to 0 to disable.
    #[serde(default = "default_max_irrecoverable_repeats")]
    pub max_irrecoverable_repeats: u32,

    /// RFC #776 Part B.4 — threshold for repeated spawn identity. When a
    /// parent spawns the same agent with the same contract + input this
    /// many times, the LoopGuard trips `RepeatedSpawnIdentity`. 0 disables.
    #[serde(default = "default_max_spawn_identity_repeats")]
    pub max_spawn_identity_repeats: u32,
}

fn default_progress_budget_tools() -> HashMap<String, u32> {
    [
        ("knowledge_store".to_string(), 3u32),
        ("knowledge_search".to_string(), 3u32),
        // workflow_wait is a polling tool — each "still running" result
        // shouldn't indefinitely reset the no-progress counter. After 3
        // successful waits, further waits stop resetting current_loops.
        ("workflow_wait".to_string(), 3u32),
    ]
    .into_iter()
    .collect()
}

impl Default for LoopGuardConfig {
    fn default() -> Self {
        Self {
            max_loops_without_progress: default_max_loops_without_progress(),
            max_tool_failures: default_max_tool_failures(),
            max_consecutive_same_progress: default_max_consecutive_same_progress(),
            max_child_failures: default_max_child_failures(),
            progress_budget_tools: default_progress_budget_tools(),
            rotation_window_size: default_rotation_window_size(),
            rotation_distinct_floor: default_rotation_distinct_floor(),
            roster_repeat_floor: default_roster_repeat_floor(),
            max_llm_failures: default_max_llm_failures(),
            child_failure_loop_penalty: default_child_failure_loop_penalty(),
            recurring_error_window: default_recurring_error_window(),
            recurring_error_distinct_tools: default_recurring_error_distinct_tools(),
            max_irrecoverable_repeats: default_max_irrecoverable_repeats(),
            max_spawn_identity_repeats: default_max_spawn_identity_repeats(),
        }
    }
}

fn default_max_loops_without_progress() -> u32 {
    10
}

fn default_max_tool_failures() -> u32 {
    8
}

fn default_max_consecutive_same_progress() -> u32 {
    1
}

fn default_max_child_failures() -> u32 {
    5
}

fn default_rotation_window_size() -> usize {
    16
}

fn default_rotation_distinct_floor() -> usize {
    6
}

fn default_roster_repeat_floor() -> u32 {
    3
}

fn default_max_llm_failures() -> u32 {
    3
}

fn default_child_failure_loop_penalty() -> u32 {
    2
}

fn default_recurring_error_window() -> usize {
    10
}

fn default_recurring_error_distinct_tools() -> usize {
    3
}

fn default_max_irrecoverable_repeats() -> u32 {
    3
}

fn default_max_spawn_identity_repeats() -> u32 {
    3
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiProposalConfig {
    /// Auto-expiry TTL in seconds for pending wiki proposals. 0 = disabled.
    /// Expired proposals are automatically cancelled.
    #[serde(default = "default_wiki_proposal_auto_expire_secs")]
    pub auto_expire_secs: u64,

    /// Enable advisory quality heuristics (warn but don't block).
    #[serde(default = "default_true")]
    pub quality_heuristics_enabled: bool,

    /// Minimum content length in characters for advisory warning.
    #[serde(default = "default_wiki_proposal_min_content_length")]
    pub min_content_length: usize,

    /// Minimum number of markdown headings (# ) for advisory warning.
    #[serde(default = "default_wiki_proposal_min_headings")]
    pub min_headings: usize,

    /// Enable duplicate detection against existing wiki pages.
    #[serde(default = "default_true")]
    pub duplicate_detection_enabled: bool,

    /// Jaccard similarity threshold for flagging duplicates (0.0–1.0).
    #[serde(default = "default_wiki_proposal_duplicate_threshold")]
    pub duplicate_threshold: f64,
}

fn default_wiki_proposal_auto_expire_secs() -> u64 {
    604800
}

fn default_wiki_proposal_min_content_length() -> usize {
    100
}

fn default_wiki_proposal_min_headings() -> usize {
    1
}

fn default_wiki_proposal_duplicate_threshold() -> f64 {
    0.7
}

impl Default for WikiProposalConfig {
    fn default() -> Self {
        Self {
            auto_expire_secs: default_wiki_proposal_auto_expire_secs(),
            quality_heuristics_enabled: true,
            min_content_length: default_wiki_proposal_min_content_length(),
            min_headings: default_wiki_proposal_min_headings(),
            duplicate_detection_enabled: true,
            duplicate_threshold: default_wiki_proposal_duplicate_threshold(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRoomConfig {
    #[serde(default)]
    pub role_floors: HashMap<String, String>,
}

impl Default for SessionRoomConfig {
    fn default() -> Self {
        Self {
            role_floors: HashMap::new(),
        }
    }
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
            require_approval_for: vec![
                "NetworkAccess".to_string(),
                "CodeExecution".to_string(),
                "ArtifactExecution".to_string(),
            ],
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

fn default_default_orchestrator() -> String {
    "planner.default".to_string()
}

fn default_port() -> u16 {
    4000
}

fn default_http_port() -> u16 {
    4100
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

fn default_standalone_approval_timeout_secs() -> u64 {
    86400
}

fn default_interaction_timeout_secs() -> u64 {
    86400
}

fn default_escalation_timeout_secs() -> u64 {
    86400
}

fn default_plan_frame_timeout_secs() -> u64 {
    86400
}

fn default_max_pending_approvals_per_root() -> usize {
    50
}

fn default_max_pending_escalations_per_root() -> usize {
    50
}

fn default_max_pending_anomaly_flags_per_reporter() -> usize {
    50
}

fn default_grant_ttl_secs() -> u64 {
    86400
}

fn default_escape_attempt_degrade_threshold() -> usize {
    5
}

fn default_escape_attempt_emergency_threshold() -> usize {
    20
}

fn default_workflow_task_heartbeat_secs_val() -> Option<u64> {
    None
}

fn default_stuck_task_timeout_secs_val() -> Option<u64> {
    Some(600)
}

fn default_max_session_turns() -> u32 {
    25
}

fn default_signal_delivery_timeout_secs() -> u64 {
    60
}

fn default_workflow_wait_secs() -> u64 {
    30
}

fn default_workflow_wait_max_total_secs() -> u64 {
    300
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

    /// DEPRECATED — no longer used; tool schemas are never compressed.
    ///
    /// Stripping tool JSON schemas to a minimal `{"type": "object"}` placeholder after turn 0 corrupted tool-calling
    /// (the model needs the full schema on every turn; prompt caching is a
    /// billing optimization, not a "remember the tools" mechanism). The
    /// governor's schema-compression strategy was removed for the same reason.
    /// Tool tokens are now saved losslessly via provider tool-array caching
    /// (see `prompt_cache_enabled`). Retained only for config backward-compat.
    #[serde(default)]
    #[deprecated(
        since = "0.x",
        note = "no longer used; tool schemas are never compressed. Retained for config backward-compat."
    )]
    pub compress_tool_schemas_after_turn_0: bool,

    /// Maximum number of tool definitions to send to the LLM per turn.
    /// 0 = unlimited. When the deduplicated tool list exceeds this cap,
    /// lower-tier tools are dropped first (Specialized → Workflow → Core).
    #[serde(default)]
    pub max_tool_definitions: usize,

    /// When true, root sessions start with Core + Workflow tools only.
    /// After the first tool call that would require a Specialized tool,
    /// the session escalates to all tiers for the rest of its lifetime.
    #[serde(default)]
    pub progressive_tool_disclosure: bool,

    /// Soft token budget that triggers the context governor *before* the hard
    /// context-window limit is reached. When set, the governor proactively
    /// summarizes/capsules old turns once `total_tokens` exceeds this value,
    /// even if the session is still far from `context_window_tokens - margin_tokens`.
    ///
    /// This is useful for large context-window models (e.g. 200K tokens) where
    /// waiting for the hard limit wastes tokens on every round. Recommended
    /// value for such models: 30000–50000.
    ///
    /// `None` (the default) disables the soft budget; only the hard limit fires.
    #[serde(default)]
    pub soft_budget_tokens: Option<u32>,

    /// Strip `reasoning_content` / `reasoning_details` from assistant messages
    /// before sending them to the LLM. The model does not need to re-read its
    /// own chain-of-thought; stripping it saves tokens without losing the
    /// reasoning in storage (checkpoints, exports, timeline events keep it).
    ///
    /// **Disabled by default.** Many thinking/reasoning models (DeepSeek,
    /// OpenRouter reasoning models, and other OpenAI-compatible thinking
    /// models) require the reasoning blocks to be replayed on subsequent
    /// turns; stripping them breaks chain-of-thought continuity. Operators
    /// whose model does not require replay can enable this to save tokens.
    #[serde(default = "default_strip_reasoning")]
    pub strip_reasoning_from_request: bool,

    /// Maximum characters to allow in a tool-result message content before it
    /// is truncated to `head + "[... N chars truncated ...]" + tail` for the
    /// LLM request. Large stdout/stderr/tool outputs are common and do not need
    /// to be replayed in full on every turn. The full result is still stored.
    /// Set to 0 to disable truncation. Default is 2000.
    #[serde(default = "default_max_tool_result_chars")]
    pub max_tool_result_chars: usize,

    /// Collapse consecutive duplicate tool-result messages into a short marker
    /// for the LLM request. Re-reading artifacts, polling `approval.status`, or
    /// repeated `workflow.state` snapshots often produce identical output across
    /// turns; replaying the full content every round wastes tokens without
    /// adding information. The first occurrence is kept; later consecutive
    /// duplicates are replaced with a reference marker. The full results are
    /// still stored. Enabled by default.
    #[serde(default = "default_dedup_tool_results")]
    pub dedup_tool_results: bool,

    /// Collapse *recurring errors* (issue #705) in the LLM request. Unlike
    /// `dedup_tool_results` (byte-identical, consecutive), this fingerprints the
    /// error text (volatile ids/timestamps/numbers stripped) so the same
    /// root-cause failure surfacing non-consecutively — the hallmark of an
    /// install/spawn death spiral — is collapsed to a marker on all but its most
    /// recent occurrence. The full results are still stored. Enabled by default.
    #[serde(default = "default_collapse_repeated_errors")]
    pub collapse_repeated_errors: bool,

    /// Mark the stable leading portion of the system prompt (foundation
    /// doctrine + SKILL instructions + guidance + output contract) as a
    /// provider prompt-cache prefix. Cache-capable drivers (Anthropic; and
    /// OpenRouter when routing Claude/Gemini) attach
    /// `cache_control: {type: ephemeral}` to that prefix so repeated turns in a
    /// session re-read it at cache rates instead of full price; the volatile
    /// per-turn tail (state attestation, degradation notice) is never cached.
    /// OpenAI and llama.cpp reuse a stable prefix automatically regardless.
    /// Enabled by default.
    #[serde(default = "default_prompt_cache_enabled")]
    pub prompt_cache_enabled: bool,

    /// Override the chars-per-token ratio used by the prompt budget
    /// estimator. `None` (the default) means "use the built-in default of
    /// 3.0 chars/token". Operators running a tokenizer that is known to
    /// behave differently (e.g. a quantized local model that splits short
    /// identifiers aggressively) can pin a different value here.
    ///
    /// Out-of-range values are clamped at the gateway to the range
    /// `[0.5, 16.0]`; non-finite or non-positive values are silently
    /// treated as "use the default".
    #[serde(default)]
    pub chars_per_token: Option<f64>,
}

fn default_prompt_budget_warn_pct() -> f64 {
    80.0
}

fn default_prompt_budget_margin() -> usize {
    4096
}

fn default_strip_reasoning() -> bool {
    false
}

fn default_max_tool_result_chars() -> usize {
    4000
}

fn default_dedup_tool_results() -> bool {
    true
}

fn default_collapse_repeated_errors() -> bool {
    true
}

fn default_prompt_cache_enabled() -> bool {
    true
}

impl Default for PromptBudgetConfig {
    fn default() -> Self {
        Self {
            system_prompt_max_tokens: 0,
            tool_definitions_max_tokens: 0,
            warn_at_pct: default_prompt_budget_warn_pct(),
            margin_tokens: default_prompt_budget_margin(),
            #[allow(deprecated)]
            compress_tool_schemas_after_turn_0: false,
            max_tool_definitions: 0,
            progressive_tool_disclosure: false,
            soft_budget_tokens: None,
            strip_reasoning_from_request: default_strip_reasoning(),
            max_tool_result_chars: default_max_tool_result_chars(),
            dedup_tool_results: default_dedup_tool_results(),
            collapse_repeated_errors: default_collapse_repeated_errors(),
            prompt_cache_enabled: default_prompt_cache_enabled(),
            chars_per_token: None,
        }
    }
}

/// Configuration for the in-session divergence monitor (Sentinel P1).
///
/// The monitor recomputes a `TrajectoryHealth` verdict every turn and
/// emits `divergence.*` causal events on level transitions only —
/// healthy sessions produce no events. Thresholds below carry the
/// design-doc defaults (`docs/design/divergence-sentinel-design.md` §4).
///
/// Per-signal toggles let operators silence noisy signals without
/// disabling the monitor outright.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryConfig {
    /// Master switch. When `false`, the monitor does not run and no
    /// `divergence.*` events are emitted.
    #[serde(default = "default_trajectory_enabled")]
    pub enabled: bool,

    /// Size of the sliding window. For `error_burst` this limits the number
    /// of turns tracked; for `repetition_entropy` it limits the number of
    /// individual tool-observation fingerprints (multiple per turn).
    #[serde(default = "default_trajectory_window")]
    pub window_size: usize,

    /// Per-signal switches. A signal disabled here is never aggregated
    /// into `TrajectoryHealth`.
    #[serde(default)]
    pub signals: TrajectorySignalsToggle,

    /// `digest_stall` thresholds in turns.
    #[serde(default)]
    pub digest_stall: TrajectoryDigestStallConfig,

    /// `repetition_entropy` thresholds in bits.
    #[serde(default)]
    pub repetition_entropy: TrajectoryRepetitionEntropyConfig,

    /// `error_burst` thresholds in error count over the window.
    #[serde(default)]
    pub error_burst: TrajectoryErrorBurstConfig,

    /// `context_pressure` thresholds as utilization fraction `[0.0, 1.0]`.
    #[serde(default)]
    pub context_pressure: TrajectoryContextPressureConfig,

    /// When `true` (default), the monitor sends an `agent.message` to the
    /// root planner on `Diverging` and `Critical` level transitions.
    #[serde(default = "default_trajectory_notify_planner")]
    pub notify_planner: bool,

    /// When `true` (default), the monitor escalates to the operator on
    /// `Critical` level transitions via two channels:
    ///
    /// 1. A non-blocking `user_interactions` row (operator can acknowledge
    ///    via the chat TUI or REST API). The agent's turn is NOT suspended.
    /// 2. An `operator_alert` causal event with the same payload, for
    ///    durable audit-chain visibility.
    #[serde(default = "default_trajectory_notify_operator")]
    pub notify_operator: bool,

    /// Maximum turns `sentinel.suppress` can request (default 10).
    /// Serves as a safety bound on planner self-suppression.
    #[serde(default = "default_trajectory_suppress_max_turns")]
    pub suppress_max_turns: u32,

    /// RFC D.5 — suppress-on-progress grace. Number of consecutive turns with
    /// feedback being incorporated that triggers suppression.
    #[serde(default = "default_progress_grace_window")]
    pub progress_grace_window: u32,

    /// RFC D.5 — how many turns to suppress Sentinel escalation once progress
    /// grace has been earned.
    #[serde(default = "default_progress_grace_turns")]
    pub progress_grace_turns: u32,
}

fn default_trajectory_enabled() -> bool {
    true
}

fn default_trajectory_window() -> usize {
    8
}

fn default_trajectory_notify_planner() -> bool {
    true
}

fn default_trajectory_notify_operator() -> bool {
    true
}

fn default_trajectory_suppress_max_turns() -> u32 {
    10
}

fn default_progress_grace_window() -> u32 {
    2
}

fn default_progress_grace_turns() -> u32 {
    3
}

impl Default for TrajectoryConfig {
    fn default() -> Self {
        Self {
            enabled: default_trajectory_enabled(),
            window_size: default_trajectory_window(),
            signals: TrajectorySignalsToggle::default(),
            digest_stall: TrajectoryDigestStallConfig::default(),
            repetition_entropy: TrajectoryRepetitionEntropyConfig::default(),
            error_burst: TrajectoryErrorBurstConfig::default(),
            context_pressure: TrajectoryContextPressureConfig::default(),
            notify_planner: default_trajectory_notify_planner(),
            notify_operator: default_trajectory_notify_operator(),
            suppress_max_turns: default_trajectory_suppress_max_turns(),
            progress_grace_window: default_progress_grace_window(),
            progress_grace_turns: default_progress_grace_turns(),
        }
    }
}

/// Per-signal enable/disable toggles. All default to `true`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectorySignalsToggle {
    #[serde(default = "default_signal_on")]
    pub loop_pressure: bool,
    #[serde(default = "default_signal_on")]
    pub failure_pressure: bool,
    #[serde(default = "default_signal_on")]
    pub child_failure_pressure: bool,
    #[serde(default = "default_signal_on")]
    pub digest_stall: bool,
    #[serde(default = "default_signal_on")]
    pub repetition_entropy: bool,
    #[serde(default = "default_signal_on")]
    pub error_burst: bool,
    #[serde(default = "default_signal_on")]
    pub context_pressure: bool,
}

fn default_signal_on() -> bool {
    true
}

impl Default for TrajectorySignalsToggle {
    fn default() -> Self {
        Self {
            loop_pressure: true,
            failure_pressure: true,
            child_failure_pressure: true,
            digest_stall: true,
            repetition_entropy: true,
            error_burst: true,
            context_pressure: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryDigestStallConfig {
    #[serde(default = "default_digest_stall_warn_turns")]
    pub warn_turns: u32,
    #[serde(default = "default_digest_stall_critical_turns")]
    pub critical_turns: u32,
}

fn default_digest_stall_warn_turns() -> u32 {
    8
}
fn default_digest_stall_critical_turns() -> u32 {
    12
}

impl Default for TrajectoryDigestStallConfig {
    fn default() -> Self {
        Self {
            warn_turns: default_digest_stall_warn_turns(),
            critical_turns: default_digest_stall_critical_turns(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryRepetitionEntropyConfig {
    /// Warn when entropy of the last `window_size` tool fingerprints is
    /// at or below this value (in bits). Low entropy means the agent is
    /// repeating itself.
    #[serde(default = "default_repetition_entropy_warn_bits")]
    pub warn_bits: f32,
    /// Entropy at or below this (in bits) is labelled "critically low" in the
    /// divergence evidence. The repetition-entropy signal is **advisory**: it
    /// caps at `Warn` severity and never escalates a session to `Critical` on
    /// its own (so it never raises the operator divergence gate). Tool-call
    /// repetition is weak evidence of being stuck — an I/O agent such as
    /// `researcher.default` legitimately repeats fetch/search calls. The
    /// gate-worthy `Critical` verdicts come from the loop guard's semantic
    /// no-progress (P-7.19) and the error-burst signal.
    #[serde(default = "default_repetition_entropy_critical_bits")]
    pub critical_bits: f32,
    /// Minimum number of tool calls in the window before the signal is
    /// evaluated. Avoids firing on a brand-new session with only one or
    /// two calls observed.
    #[serde(default = "default_repetition_entropy_min_observations")]
    pub min_observations: usize,
    /// Warm-up: the signal is not evaluated until the session reaches this
    /// turn. Divergence is a trajectory property, not a single-turn one — an
    /// agent's opening burst of similar calls (e.g. a researcher fetching many
    /// pages in turn 1) must not trip it.
    #[serde(default = "default_repetition_entropy_min_turns")]
    pub min_turns: u64,
}

fn default_repetition_entropy_warn_bits() -> f32 {
    1.2
}
fn default_repetition_entropy_critical_bits() -> f32 {
    0.5
}
fn default_repetition_entropy_min_observations() -> usize {
    4
}
fn default_repetition_entropy_min_turns() -> u64 {
    3
}

impl Default for TrajectoryRepetitionEntropyConfig {
    fn default() -> Self {
        Self {
            warn_bits: default_repetition_entropy_warn_bits(),
            critical_bits: default_repetition_entropy_critical_bits(),
            min_observations: default_repetition_entropy_min_observations(),
            min_turns: default_repetition_entropy_min_turns(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryErrorBurstConfig {
    /// Warn when the count of error tool-results in the last
    /// `window_size` turns reaches this number.
    #[serde(default = "default_error_burst_warn_count")]
    pub warn_count: u32,
    #[serde(default = "default_error_burst_critical_count")]
    pub critical_count: u32,
}

fn default_error_burst_warn_count() -> u32 {
    8
}
fn default_error_burst_critical_count() -> u32 {
    12
}

impl Default for TrajectoryErrorBurstConfig {
    fn default() -> Self {
        Self {
            warn_count: default_error_burst_warn_count(),
            critical_count: default_error_burst_critical_count(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryContextPressureConfig {
    /// Warn when context utilization fraction reaches this value.
    #[serde(default = "default_context_pressure_warn_fraction")]
    pub warn_fraction: f32,
    #[serde(default = "default_context_pressure_critical_fraction")]
    pub critical_fraction: f32,
}

fn default_context_pressure_warn_fraction() -> f32 {
    0.80
}
fn default_context_pressure_critical_fraction() -> f32 {
    0.95
}

impl Default for TrajectoryContextPressureConfig {
    fn default() -> Self {
        Self {
            warn_fraction: default_context_pressure_warn_fraction(),
            critical_fraction: default_context_pressure_critical_fraction(),
        }
    }
}

/// Configuration for context compression.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextCompressionConfig {
    /// Enable context compression. Default: true. Requires `llm_preset` (or
    /// `provider`/`model`) to be set to a fixed cheap model preset; if no
    /// preset resolves, the capsule strategy logs a warning and skips
    /// compression for the turn (graceful no-op).
    #[serde(default = "default_compression_enabled")]
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

    /// Maximum number of capsule decisions to keep before summarization.
    /// Only used when capsule strategy is active. Default: 30
    #[serde(default = "default_max_capsule_decisions")]
    pub max_capsule_decisions: usize,

    /// Maximum number of completed capsule tasks to retain.
    /// Only used when capsule strategy is active. Default: 10
    #[serde(default = "default_max_completed_tasks")]
    pub max_completed_tasks: usize,
}

fn default_compression_enabled() -> bool {
    true
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

fn default_max_capsule_decisions() -> usize {
    30
}

fn default_max_completed_tasks() -> usize {
    10
}

impl Default for ContextCompressionConfig {
    fn default() -> Self {
        Self {
            enabled: default_compression_enabled(),
            llm_preset: None,
            provider: None,
            model: None,
            threshold_pct: default_compression_threshold_pct(),
            recent_turns_to_keep: default_compression_recent_turns(),
            max_summary_tokens: default_compression_max_summary_tokens(),
            min_turns_between_compression: default_min_turns_between_compression(),
            max_capsule_decisions: default_max_capsule_decisions(),
            max_completed_tasks: default_max_completed_tasks(),
        }
    }
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            agents_dir: default_agents_dir(),
            port: default_port(),
            http_port: default_http_port(),
            ofp_port: default_ofp_port(),
            tls: false,
            node_id: default_node_id(),
            node_name: default_node_name(),
            constitution: ConstitutionConfig::default(),
            federation_constitution: FederationConstitutionConfig::default(),
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
            default_orchestrator: default_default_orchestrator(),
            code_analysis: CodeAnalysisConfig::default(),
            capability_delta_gate_mode: CapabilityDeltaGateMode::Strict,
            allow_zero_capability_direct_promote: true,
            require_operator_approval_for_new_agents: true,
            agent_install_smoke_test: AgentInstallSmokeTestMode::Ask,
            session_budget: SessionBudgetConfig::default(),
            root_session_budget: RootSessionBudgetConfig::default(),
            approval_timeout_secs: default_approval_timeout_secs(),
            standalone_approval_timeout_secs: default_standalone_approval_timeout_secs(),
            interaction_timeout_secs: default_interaction_timeout_secs(),
            escalation_timeout_secs: default_escalation_timeout_secs(),
            plan_frame_timeout_secs: default_plan_frame_timeout_secs(),
            max_pending_approvals_per_root: default_max_pending_approvals_per_root(),
            max_pending_escalations_per_root: default_max_pending_escalations_per_root(),
            max_pending_anomaly_flags_per_reporter: default_max_pending_anomaly_flags_per_reporter(),
            default_grant_ttl_secs: default_grant_ttl_secs(),
            escape_attempt_degrade_threshold: default_escape_attempt_degrade_threshold(),
            escape_attempt_emergency_threshold: default_escape_attempt_emergency_threshold(),
            continuation_key: None,
            workflow_task_heartbeat_secs: default_workflow_task_heartbeat_secs_val(),
            stuck_task_timeout_secs: default_stuck_task_timeout_secs_val(),
            stuck_task_no_evidence_action: StuckTaskNoEvidenceAction::default(),
            evidence_mode: default_evidence_mode(),
            session_report: SessionReportConfig::default(),
            digest_agent: DigestAgentConfig::default(),
            outcome_grader: OutcomeGraderConfig::default(),
            improve: ImproveConfig::default(),
            retention: RetentionConfig::default(),
            capsule: CapsuleConfig::default(),
            reclamation: ReclamationConfig::default(),
            response_validation: ResponseValidationConfig::default(),
            validation_waivers: ValidationWaiversConfig::default(),
            sandbox: SandboxConfig::default(),
            max_session_turns: default_max_session_turns(),
            loop_guard: LoopGuardConfig::default(),
            prompt_budget: PromptBudgetConfig::default(),
            trajectory: TrajectoryConfig::default(),
            llm_routing: None,
            chat: ChatConfig::default(),
            operator_activity: OperatorActivityConfig::default(),
            decider_obligations: DeciderObligationsConfig::default(),
            approval_levels: ApprovalLevelConfig::default(),
            context_compression: ContextCompressionConfig::default(),
            signal_delivery_timeout_secs: default_signal_delivery_timeout_secs(),
            default_workflow_wait_secs: default_workflow_wait_secs(),
            workflow_wait_max_total_secs: default_workflow_wait_max_total_secs(),
            hooks: Vec::new(),
            scheduled_jobs: ScheduledJobsConfig::default(),
            promotion_governor: PromotionGovernorConfig::default(),
            fast_scheduler: FastSchedulerConfig::default(),
            system_agents: Vec::new(),
            interaction_answer_orchestration: default_interaction_answer_orchestration(),
            allow_runtime_lock_drift: false,
            plan_auto_approve: false,
            plan_auto_approver: default_plan_auto_approver(),
            trust_unsigned_bundles: false,
            approval_dwell_multiplier: default_approval_dwell_multiplier(),
            sentinel: SentinelConfig::default(),
            protected_agents: ProtectedAgentsConfig::default(),
            profile: Profile::default(),
            auto_learning: AutoLearningConfig::default(),
            persona_path: None,
            wiki_proposal: WikiProposalConfig::default(),
            session_room: SessionRoomConfig::default(),
        }
    }
}

impl GatewayConfig {
    /// Apply profile-specific defaults for knobs that use serde defaults when omitted.
    /// Explicit operator values in `config.yaml` are preserved.
    pub fn apply_profile_defaults(&mut self) {
        if matches!(self.profile, Profile::Starter) {
            if self.evidence_mode == default_evidence_mode() {
                self.evidence_mode = "errors".to_string();
            }
            // `session_report.live_html_on_update` defaults to false via serde.
        }
    }

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

        // Cross-cutting role keys require a fixed (non-routing) preset because
        // the consumers (e.g. compression LLM resolution) need a concrete
        // provider/model, not a runtime routing decision.
        if let Some(preset_name) = self.llm_preset_mapping.get("context_compression") {
            if let Some(preset) = self.llm_presets.get(preset_name) {
                if preset.routing.is_some() {
                    errors.push(format!(
                        "llm_preset_mapping.context_compression: '{}' is a routing preset; \
                         context_compression requires a fixed preset",
                        preset_name
                    ));
                }
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
    }

    #[test]
    fn prompt_budget_config_json_roundtrip() {
        let j = serde_json::json!({
            "system_prompt_max_tokens": 8000,
            "tool_definitions_max_tokens": 4000,
            "warn_at_pct": 90.0,
            "margin_tokens": 2048,
        });
        let parsed: PromptBudgetConfig = serde_json::from_value(j).expect("parse json");
        assert_eq!(parsed.system_prompt_max_tokens, 8000);
        assert_eq!(parsed.tool_definitions_max_tokens, 4000);
        assert_eq!(parsed.warn_at_pct, 90.0);
        assert_eq!(parsed.margin_tokens, 2048);
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

    #[test]
    fn validate_llm_presets_rejects_routing_preset_for_context_compression() {
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
                    classifier_preset: None,
                    deterministic: DeterministicRoutingConfig::default(),
                    classifier: ClassifierRoutingConfig::default(),
                    hybrid: HybridRoutingConfig::default(),
                }),
            },
        );
        config
            .llm_preset_mapping
            .insert("context_compression".to_string(), "smart".to_string());

        let errors = config.validate_llm_presets();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("context_compression") && e.contains("routing preset")),
            "expected routing-preset rejection for context_compression, got: {:?}",
            errors
        );
    }

    #[test]
    fn schema_enforcement_config_rejects_llm_mode() {
        let j = serde_json::json!({
            "agents_dir": "/tmp/autonoetic-agents",
            "schema_enforcement": {
                "mode": "llm",
                "audit": true
            }
        });
        let err = serde_json::from_value::<GatewayConfig>(j).unwrap_err();
        assert!(err.to_string().contains("unknown variant"));
    }

    #[test]
    fn schema_enforcement_config_rejects_llm_agent_override() {
        let j = serde_json::json!({
            "agents_dir": "/tmp/autonoetic-agents",
            "schema_enforcement": {
                "mode": "deterministic",
                "audit": true,
                "agent_overrides": {
                    "planner.default": "llm"
                }
            }
        });
        let err = serde_json::from_value::<GatewayConfig>(j).unwrap_err();
        assert!(err.to_string().contains("unknown variant"));
    }

    #[test]
    fn validation_waivers_defaults_to_disabled() {
        let config = GatewayConfig::default();
        assert!(!config.validation_waivers.enabled);
        assert!(!config.validation_waivers.auto_propose_after_reconcile);
    }

    #[test]
    fn validation_waivers_config_parses_when_omitted() {
        let j = serde_json::json!({
            "agents_dir": "/tmp/autonoetic-agents"
        });
        let parsed: GatewayConfig = serde_json::from_value(j).expect("parse json");
        assert!(!parsed.validation_waivers.enabled);
        assert!(!parsed.validation_waivers.auto_propose_after_reconcile);
    }

    #[test]
    fn validation_waivers_config_parses_when_enabled() {
        let j = serde_json::json!({
            "agents_dir": "/tmp/autonoetic-agents",
            "validation_waivers": {
                "enabled": true,
                "auto_propose_after_reconcile": true
            }
        });
        let parsed: GatewayConfig = serde_json::from_value(j).expect("parse json");
        assert!(parsed.validation_waivers.enabled);
        assert!(parsed.validation_waivers.auto_propose_after_reconcile);
    }

    #[test]
    fn decider_obligations_adjudication_sla_secs_defaults_to_seven_days() {
        assert_eq!(
            DeciderObligationsConfig::default().adjudication_sla_secs,
            604800
        );
    }
}
