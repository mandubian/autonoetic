//! Agent Manifest types — the Rust representation of `SKILL.md` frontmatter.

use crate::background::{BackgroundPolicy, GrantTarget};
use crate::disclosure::DisclosurePolicy;
use crate::egress::NamedEgressLabel;
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentIdentity {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub singleton: bool,
    /// Opt in to **residency**: when set, a session of this agent does not
    /// terminate once its task is done. It parks in the gateway's
    /// `YieldReason::Idle` checkpoint state (`autonoetic-gateway`, not linkable
    /// from this crate) and stays addressable
    /// by `agent_message` until this many seconds pass without traffic, at
    /// which point the gateway reaps it and the session closes normally.
    ///
    /// Absent (the default) means the historical behaviour: the session ends
    /// with its task and can never receive a message afterwards.
    ///
    /// Residency does **not** reuse context across tasks — each inbound message
    /// continues the same session, so an operator opting an agent in should
    /// expect accumulated history. Context reuse for *new* tasks is a separate,
    /// deferred question (stateful singleton sessions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resident_idle_ttl_secs: Option<u64>,
}

/// LLM configuration for the agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

    /// Egress (data localization) classification carried from the preset at
    /// resolve time — RFC data-envelopes §5.1. `None` means "infer from
    /// provider defaults" (ollama/vllm/lmstudio/llamacpp → local, else remote).
    /// Maps to a [`crate::egress::Sink`] at request time so the chokepoint
    /// (phase 1b #905) knows which sink a completion targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_class: Option<crate::egress::EgressClass>,

    /// Per-request timeout (seconds) carried from the preset at resolve time
    /// (#1045). `None` means "use the gateway default
    /// (`llm_request_timeout_secs`) or the built-in 120s". Lets a long-running
    /// `coding` preset outlast a `haiku` one instead of sharing one
    /// process-wide budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout_secs: Option<u64>,

    /// Time-to-first-byte budget (seconds) for the streaming turn path,
    /// carried from the preset at resolve time. `None` means "use the gateway
    /// default (`llm_ttfb_timeout_secs`), or — when that is unset too — share
    /// `request_timeout_secs`", preserving the single-budget behavior. An
    /// overloaded provider can queue a request far longer than any legitimate
    /// mid-stream silence, so the queue wait gets its own budget instead of
    /// forcing operators to also tolerate equally long mid-stream gaps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttfb_timeout_secs: Option<u64>,
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

fn is_false(v: &bool) -> bool {
    !*v
}

/// Per-agent overrides merged onto a resolved `llm_preset` at runtime.
/// Must not carry `provider` / `model` — those live in gateway `llm_presets`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct LlmOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<u32>,
}

/// Resource limits enforced by the Gateway.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub max_memory_mb: u64,
    pub max_execution_time_sec: u64,
    pub token_budget_monthly: Option<u64>,
}

/// Per-agent egress (data-localization) manifest — RFC data-envelopes §4.1 path 2.
///
/// Declares the bundle-wide **output floor**: the most restrictive label the
/// bundle's own outputs may carry. A floor can only **restrict** — it
/// intersects into every resolution alongside operator rules and can never
/// widen what operator policy already restricted. Declared under
/// `metadata.autonoetic.egress.output_label` in `SKILL.md`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentEgressManifest {
    /// The named output floor for this bundle's tool results
    /// (e.g. `local_only` for an email-reading bundle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_label: Option<NamedEgressLabel>,
}

impl AgentEgressManifest {
    /// Whether this manifest declares anything (no floor → no-op).
    pub fn is_empty(&self) -> bool {
        self.output_label.is_none()
    }
}

/// The full parsed Agent Manifest (SKILL.md frontmatter).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// Optional fields merged onto the resolved preset (temperature, thinking, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_overrides: Option<LlmOverrides>,
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
    /// Tool name patterns to exclude from this agent's tool set, applied
    /// after capability gating (`is_available`) and before tier filtering.
    /// Supports glob wildcards: `workbench_*`, `scheduler_*`, `eval_*`.
    /// Useful for trimming the tool surface when an agent's capabilities
    /// unlock tools it never needs (e.g., a coder doesn't need `planframe_*`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_tools: Vec<String>,
    /// Per-section gates for the `SKILL.md` body (RFC
    /// `prompt-burden-phase-gated-guidance`, P3). Each entry names a top-level
    /// `##` heading and the session phase that must be reached before that
    /// section enters the prompt. Ungated sections are always present.
    ///
    /// Declared in frontmatter rather than as inline markers so a gate cannot
    /// silently drift from a renamed heading: both an unknown heading and an
    /// unrecognised phase fact fail at parse time rather than by prose quietly
    /// going missing at runtime.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<SectionGate>,
    /// Metadata from AgentSkills.io import. Set when the agent was imported
    /// from an external AgentSkills-compatible SKILL.md. Used for tool name
    /// bridging, resource mounting, and trust mode enforcement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agentskills_import: Option<AgentSkillsImportMetadata>,
    /// Per-agent context compression override. When present, enables compression
    /// for this agent with optional overrides for threshold and LLM preset.
    #[serde(default)]
    pub compression: Option<CompressionConfig>,
    /// When true, the agent is a genuine open-web agent and may declare
    /// `NetworkAccess.hosts: ["*"]` at install time.
    #[serde(default, skip_serializing_if = "is_false")]
    pub open_web: bool,
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
    /// Per-agent egress (data-localization) manifest — RFC data-envelopes §4.1
    /// path 2. When present, the bundle-declared output floor is intersected
    /// into every label resolution in this session, alongside operator rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<AgentEgressManifest>,
}

fn is_default_sandbox_network_policy(p: &SandboxNetworkPolicy) -> bool {
    matches!(p, SandboxNetworkPolicy::Normal)
}

/// Agent-declared remote-access patterns used for deterministic gateway checks.
///
/// `deny_unknown_fields` is intentional: a misspelled or invented field (e.g.
/// `hosts:`/`patterns:` instead of `targets:`/`function_calls:`) must FAIL to
/// parse loudly rather than silently becoming an empty declaration — a silent
/// no-op leaves the agent's sandbox exec blocked at runtime with no actionable
/// signal (the root cause of the session-912c7791 imaplib thrash). Install-time
/// validation surfaces the precise error.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    pub targets: Vec<GrantTarget>,
    /// Optional language allowlist for the remote-access analyzer.
    ///
    /// When set it scopes *both* halves of static detection to these languages:
    /// which import detectors run, and which language-tagged function-call
    /// heuristics run (`axios.*`/`fetch(` are JavaScript-only, `urlopen(` is
    /// Python-only, `reqwest::get(` is Rust-only, …). Language-agnostic call
    /// patterns — socket primitives like `.connect(`/`.send(` — always run.
    ///
    /// Empty means no allowlist: every import detector runs, and the
    /// function-call scope falls back to the languages implied by the code's own
    /// import signals (with no such signal, every tagged pattern runs).
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

/// Optional per-agent circuit-breaker limits declared in SKILL metadata, or
/// derived from the agent's execution shape.
///
/// Explicit manifest declarations are applied as stricter bounds within system
/// ceilings (operator-controlled safety). Role-aware defaults derived from
/// `execution_mode` are applied directly, so deterministic executors can be
/// granted more headroom than the global reasoning defaults.
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
    /// Per-agent session turn limit override (clamped to system ceiling).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_session_turns: Option<u32>,
    /// Per-agent absolute session-turn ceiling override (clamped to the system
    /// `max_session_turns_hard`). Unlike `max_session_turns` this ceiling
    /// **cannot** be lifted by a continuation approval — only emergency-stop or
    /// operator revoke. When unset it defaults to `2 ×` the effective soft
    /// limit (clamped to the system ceiling).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_session_turns_hard: Option<u32>,
}

impl LoopGuardDeclaration {
    pub fn for_execution_mode(mode: ExecutionMode) -> Self {
        match mode {
            ExecutionMode::Reasoning => Self::default(),
            ExecutionMode::Script => Self {
                max_loops_without_progress: Some(15),
                max_tool_failures: Some(12),
                max_consecutive_same_progress: Some(1),
                max_child_failures: Some(5),
                max_session_turns: None,
                max_session_turns_hard: None,
            },
        }
    }
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

/// Explicit session lifecycle state (issue #742, centralized in #1057).
///
/// Replaces the overloaded transcript `status = "completed"` which conflated
/// "session terminated normally" with "session hibernated between turns".
/// The orphan reaper (R+12) and `try_complete_workflow` read this single field
/// instead of inferring lifecycle position from transcript status plus auxiliary
/// heuristics.
///
/// This enum is the **single owner** of the `session_transcripts.lifecycle_state`
/// vocabulary. The gateway writes one of these values (via [`Self::as_str`]) and
/// every reader asks this enum how to classify a state — [`Self::is_terminal`],
/// [`Self::is_resumable`], [`Self::permits_workflow_completion`] — instead of
/// restating a subset of the vocabulary as ad-hoc SQL/Rust string literals.
/// Adding a variant is a compile error at every classification site (each method
/// is an exhaustive match), and the SQL-literal helpers below are cross-checked
/// against `is_resumable` by `resumable_sql_list_matches_is_resumable`. That
/// mechanical link is the point: a new state can no longer be silently
/// misclassified by a reader in another file.
///
/// One asymmetry is deliberate. [`FromStr`](std::str::FromStr) is strict — it
/// knows only this build's vocabulary — because a writer must never invent a
/// value. Readers use [`Self::classify_stored`] instead, which honours the
/// [`Self::TERMINATED_PREFIX`] contract so a row written by a newer gateway is
/// still classified correctly. Strictness on the read path would re-introduce
/// exactly the failure this enum exists to prevent, one version boundary out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLifecycleState {
    /// Session is actively executing a turn.
    Active,
    /// Session completed its turn and is hibernated (between turns, will resume
    /// on the next operator message).
    Hibernated,
    /// Session is suspended awaiting a gate (approval, user input, escalation).
    AwaitingGate,
    /// Resident session that finished its task and is parked only so peers can
    /// still reach it (#902). Nothing is being waited on; the session resumes on
    /// the next inbound message and is reaped once its idle TTL elapses.
    Idle,
    /// Operator-initiated cooperative pause (#1026): the turn parked at a tool
    /// boundary via `root_session.pause`. Distinct from `Hibernated` — that means
    /// *the agent* finished, this means *the operator* paused. Resumes
    /// (hibernate-like) on the next message.
    Paused,
    /// Session has terminated (completed / failed / suspended terminal).
    Terminated(TerminatedReason),
}

impl SessionLifecycleState {
    /// Stable wire/SQL representation. Used by every writer so the on-disk
    /// vocabulary has one source.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Hibernated => "hibernated",
            Self::AwaitingGate => "awaiting_gate",
            Self::Idle => "idle",
            Self::Paused => "paused",
            Self::Terminated(TerminatedReason::Completed) => "terminated:completed",
            Self::Terminated(TerminatedReason::Failed) => "terminated:failed",
            Self::Terminated(TerminatedReason::Suspended) => "terminated:suspended",
        }
    }

    /// A truly-ended session that will never resume. Only `Terminated`.
    ///
    /// Read by `find_orphaned_sessions` (a terminated parent orphans its
    /// children) and `terminate_session_transcript`'s idempotency guard.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminated(_))
    }

    /// A parked session that is expected to resume, and so must be protected
    /// from the "polite" finalize path (`finalize_session_transcript`), which
    /// cannot tell a between-turn yield from a real end. `Hibernated`,
    /// `AwaitingGate`, `Idle`, and `Paused` are resumable; `Active` is not (an
    /// active session being finalized *is* ending); `Terminated` is not.
    ///
    /// Read by `finalize_session_transcript`'s `NOT IN (...)` guard.
    pub fn is_resumable(&self) -> bool {
        matches!(
            self,
            Self::Hibernated | Self::AwaitingGate | Self::Idle | Self::Paused
        )
    }

    /// Whether a root session in this state permits its workflow to complete.
    ///
    /// The workflow join fires when the session has no further planned work:
    /// `Terminated` (done for good), `Hibernated` (between turns — all join /
    /// active / queued conditions are independently checked beforehand), and
    /// `Idle` (finished its task, parked only for peer reachability). `Paused`
    /// and `AwaitingGate` block completion — the operator may redirect, or a
    /// gate is pending; `Active` blocks it (still running).
    ///
    /// Read by `try_complete_workflow`.
    pub fn permits_workflow_completion(&self) -> bool {
        matches!(
            self,
            Self::Terminated(_) | Self::Hibernated | Self::Idle
        )
    }

    /// The shared prefix of every terminal state. This is a **contract**, not a
    /// spelling convention: readers classify terminalness by this prefix so a
    /// `terminated:<reason>` this build has never heard of is still terminal.
    /// Held to by `every_terminal_state_carries_the_terminated_prefix`.
    pub const TERMINATED_PREFIX: &'static str = "terminated:";

    /// SQL `LIKE` pattern matching every terminal state — the SQL spelling of
    /// [`Self::TERMINATED_PREFIX`], cross-checked against it by
    /// `resumable_sql_list_matches_is_resumable`.
    pub const TERMINATED_SQL_PREFIX: &'static str = "terminated:%";

    /// SQL fragment: a comma-separated, single-quoted list of every resumable
    /// state, suitable for `lifecycle_state NOT IN (...)`. Static literals only
    /// — no interpolation — so there is no injection surface. Cross-checked
    /// against `is_resumable` by `resumable_sql_list_matches_is_resumable` so a
    /// newly added resumable variant that is forgotten here fails the build.
    pub const RESUMABLE_SQL_LIST: &'static str = "'hibernated','awaiting_gate','idle','paused'";

    /// Classify a value **read back from the store**.
    ///
    /// [`FromStr`](std::str::FromStr) is deliberately strict: it round-trips
    /// only the vocabulary this binary knows. Reads cannot afford to be. A row
    /// may have been written by a newer gateway sharing the same database, or
    /// by a [`TerminatedReason`] added after this binary was built — and while
    /// adding a reason is a compile error in [`Self::as_str`], it is *not* one
    /// in `FromStr`, whose `_ => Err` arm absorbs it silently.
    ///
    /// Terminal states share [`Self::TERMINATED_PREFIX`], and that prefix — not
    /// the exact reason — is what readers classified on before #1057
    /// centralized the vocabulary. Treating an unknown `terminated:<reason>` as
    /// non-terminal would mean its children are never orphaned and its workflow
    /// never completes: the #1056 livelock, re-entered through a strict parse.
    ///
    /// Genuinely unrecognised values stay conservative — see
    /// [`StoredLifecycle::Unrecognised`].
    pub fn classify_stored(raw: &str) -> StoredLifecycle {
        match raw.parse::<Self>() {
            Ok(state) => StoredLifecycle::Known(state),
            Err(_) if raw.starts_with(Self::TERMINATED_PREFIX) => StoredLifecycle::UnknownTerminal,
            Err(_) => StoredLifecycle::Unrecognised,
        }
    }
}

/// The classification of a `lifecycle_state` value read out of the store —
/// the read-side counterpart to [`SessionLifecycleState`].
///
/// Writes always go through [`SessionLifecycleState::as_str`] and so are always
/// in this build's vocabulary; reads are not. Produced by
/// [`SessionLifecycleState::classify_stored`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredLifecycle {
    /// A value this build knows. Classification delegates to the enum.
    Known(SessionLifecycleState),
    /// `terminated:<reason>` with a reason this build does not know — terminal
    /// by the prefix contract, so a forward-written row still reaps its
    /// children and releases its workflow.
    UnknownTerminal,
    /// Not in the vocabulary at all: corrupt, or a *non-terminal* state from a
    /// newer gateway. Classified conservatively — not terminal (children stay
    /// protected) and it blocks workflow completion. Unlike `UnknownTerminal`
    /// there is no signal here to act on, so the safe reading is "still alive".
    Unrecognised,
}

impl StoredLifecycle {
    /// A truly-ended session that will never resume. See
    /// [`SessionLifecycleState::is_terminal`].
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Known(state) => state.is_terminal(),
            Self::UnknownTerminal => true,
            Self::Unrecognised => false,
        }
    }

    /// Whether a root session in this state permits its workflow to complete.
    /// See [`SessionLifecycleState::permits_workflow_completion`].
    pub fn permits_workflow_completion(&self) -> bool {
        match self {
            Self::Known(state) => state.permits_workflow_completion(),
            Self::UnknownTerminal => true,
            Self::Unrecognised => false,
        }
    }
}

impl std::fmt::Display for SessionLifecycleState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SessionLifecycleState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Self::Active),
            "hibernated" => Ok(Self::Hibernated),
            "awaiting_gate" => Ok(Self::AwaitingGate),
            "idle" => Ok(Self::Idle),
            "paused" => Ok(Self::Paused),
            "terminated:completed" => Ok(Self::Terminated(TerminatedReason::Completed)),
            "terminated:failed" => Ok(Self::Terminated(TerminatedReason::Failed)),
            "terminated:suspended" => Ok(Self::Terminated(TerminatedReason::Suspended)),
            _ => Err(format!("invalid SessionLifecycleState: {s}")),
        }
    }
}

/// The reason a terminated session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminatedReason {
    Completed,
    Failed,
    Suspended,
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
/// Each step is executed server-side by the gateway during `credential_setup`.
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
    /// Tells `credential_setup` to pause and ask the agent to collect user input.
    ///
    /// When the gateway encounters this step it returns early with
    /// `suspended_for_user_input: true` and the question.  The agent should call
    /// `user_ask` with the question, collect the human's answer, then call
    /// `credential_setup` again with `credential_id` + `resume_vars: { var_name: answer }`.
    ///
    /// `secret_fields` is rejected at execution time: `user_input` answers flow
    /// through the LLM (`user_ask`), so it must never be used to collect secret
    /// material — use [`CredentialSetupStep::UserPrompt`] for secrets, which
    /// prompts the operator through a secure out-of-band channel.
    UserInput {
        question: String,
        var_name: String,
        #[serde(default)]
        secret_fields: Vec<SecretFieldSpec>,
    },
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

/// A gate on one top-level section of a `SKILL.md` body (RFC P3).
///
/// The `<!-- extended -->` marker it generalizes is all-or-nothing and *defers*
/// by a single turn — the extended half is inlined permanently from turn 2. A
/// section gate **evicts**: the section stays out of the prompt entirely until
/// the session reaches the named phase, so a planner that never builds anything
/// never pays for the federation doctrine at all.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SectionGate {
    /// Exact text of the top-level `##` heading this gates, without the `##`.
    pub heading: String,
    /// The gate expression. Currently only `phase(<fact>)`, e.g.
    /// `phase(artifact_built)`. Validated at parse time against the known fact
    /// vocabulary, so a typo fails loudly instead of silently never firing.
    pub when: String,
}

impl SectionGate {
    /// The phase fact this gate requires, or `None` if `when` is not a
    /// well-formed `phase(...)` expression.
    pub fn phase_fact(&self) -> Option<&str> {
        let trimmed = self.when.trim();
        let inner = trimmed
            .strip_prefix("phase(")
            .and_then(|rest| rest.strip_suffix(')'))?;
        let fact = inner.trim();
        (!fact.is_empty()).then_some(fact)
    }
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
    /// True when `manifest.capabilities` was *inferred* from `allowed-tools`
    /// rather than explicitly declared under
    /// `metadata.autonoetic.capabilities`. Recorded at the single place that
    /// knows (the parser), so downstream trust decisions (e.g.
    /// `skill_install`'s strict-mode high-risk clamp) never have to guess
    /// from the shape of the capability set.
    #[serde(default)]
    pub capabilities_inferred: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_state_display_and_parse() {
        let cases = vec![
            (SessionLifecycleState::Active, "active"),
            (SessionLifecycleState::Hibernated, "hibernated"),
            (SessionLifecycleState::AwaitingGate, "awaiting_gate"),
            (SessionLifecycleState::Idle, "idle"),
            (SessionLifecycleState::Paused, "paused"),
            (SessionLifecycleState::Terminated(TerminatedReason::Completed), "terminated:completed"),
            (SessionLifecycleState::Terminated(TerminatedReason::Failed), "terminated:failed"),
            (SessionLifecycleState::Terminated(TerminatedReason::Suspended), "terminated:suspended"),
        ];
        for (state, expected) in cases {
            assert_eq!(state.to_string(), expected);
            assert_eq!(state.as_str(), expected);
            let parsed: SessionLifecycleState = expected.parse().unwrap();
            assert_eq!(parsed, state);
        }
    }

    #[test]
    fn lifecycle_state_classification() {
        use SessionLifecycleState as S;
        let term = [
            S::Terminated(TerminatedReason::Completed),
            S::Terminated(TerminatedReason::Failed),
            S::Terminated(TerminatedReason::Suspended),
        ];
        for s in [S::Active, S::Hibernated, S::AwaitingGate, S::Idle, S::Paused] {
            assert!(!s.is_terminal(), "{s:?} must not be terminal");
        }
        for s in term {
            assert!(s.is_terminal(), "{s:?} must be terminal");
        }

        // Resumable (protected from the polite finalize path).
        for s in [S::Hibernated, S::AwaitingGate, S::Idle, S::Paused] {
            assert!(s.is_resumable(), "{s:?} must be resumable");
        }
        for s in [
            S::Active,
            S::Terminated(TerminatedReason::Completed),
            S::Terminated(TerminatedReason::Failed),
        ] {
            assert!(!s.is_resumable(), "{s:?} must not be resumable");
        }

        // Workflow completion gate.
        for s in [
            S::Hibernated,
            S::Idle,
            S::Terminated(TerminatedReason::Completed),
            S::Terminated(TerminatedReason::Failed),
        ] {
            assert!(
                s.permits_workflow_completion(),
                "{s:?} must permit workflow completion"
            );
        }
        for s in [S::Active, S::AwaitingGate, S::Paused] {
            assert!(
                !s.permits_workflow_completion(),
                "{s:?} must block workflow completion"
            );
        }
    }

    /// The SQL literal helpers must stay in lock-step with the classification
    /// methods — this is the mechanical link that turns a forgotten update into
    /// a failing test rather than a silent misclassification.
    #[test]
    fn resumable_sql_list_matches_is_resumable() {
        let all = [
            SessionLifecycleState::Active,
            SessionLifecycleState::Hibernated,
            SessionLifecycleState::AwaitingGate,
            SessionLifecycleState::Idle,
            SessionLifecycleState::Paused,
            SessionLifecycleState::Terminated(TerminatedReason::Completed),
            SessionLifecycleState::Terminated(TerminatedReason::Failed),
            SessionLifecycleState::Terminated(TerminatedReason::Suspended),
        ];
        for s in all {
            let quoted = format!("'{}'", s.as_str());
            let in_list = SessionLifecycleState::RESUMABLE_SQL_LIST.contains(&quoted);
            assert_eq!(
                in_list,
                s.is_resumable(),
                "RESUMABLE_SQL_LIST membership for {s:?} ({quoted}) disagrees with is_resumable()"
            );
            if s.is_terminal() {
                let pat = SessionLifecycleState::TERMINATED_SQL_PREFIX;
                assert!(
                    s.as_str().starts_with(pat.trim_end_matches('%')),
                    "{s:?} is terminal but as_str() {:?} does not match the TERMINATED_SQL_PREFIX {:?}",
                    s.as_str(),
                    pat
                );
            }
        }
    }

    #[test]
    fn lifecycle_state_invalid_parse_fails() {
        let result: Result<SessionLifecycleState, String> = "bogus".parse();
        assert!(result.is_err());
    }

    /// The prefix is a contract the read path depends on: `classify_stored`
    /// calls a `terminated:*` value terminal without knowing its reason. A
    /// terminal state spelled any other way would be silently misread as alive
    /// by any build that doesn't know it.
    #[test]
    fn every_terminal_state_carries_the_terminated_prefix() {
        for reason in [
            TerminatedReason::Completed,
            TerminatedReason::Failed,
            TerminatedReason::Suspended,
        ] {
            let s = SessionLifecycleState::Terminated(reason);
            assert!(
                s.as_str()
                    .starts_with(SessionLifecycleState::TERMINATED_PREFIX),
                "{s:?} is terminal but as_str() {:?} does not carry TERMINATED_PREFIX {:?}",
                s.as_str(),
                SessionLifecycleState::TERMINATED_PREFIX
            );
        }
        // The SQL pattern must be the same prefix, plus the LIKE wildcard.
        assert_eq!(
            SessionLifecycleState::TERMINATED_SQL_PREFIX,
            format!("{}%", SessionLifecycleState::TERMINATED_PREFIX)
        );
        // No non-terminal state may borrow the prefix, or it would be read as
        // terminal by a build that doesn't know it.
        for s in [
            SessionLifecycleState::Active,
            SessionLifecycleState::Hibernated,
            SessionLifecycleState::AwaitingGate,
            SessionLifecycleState::Idle,
            SessionLifecycleState::Paused,
        ] {
            assert!(
                !s.as_str()
                    .starts_with(SessionLifecycleState::TERMINATED_PREFIX),
                "{s:?} is not terminal but carries the terminal prefix"
            );
        }
    }

    /// Round-trip: every known value classifies as `Known` and back to itself.
    #[test]
    fn classify_stored_recognises_the_whole_known_vocabulary() {
        for s in [
            SessionLifecycleState::Active,
            SessionLifecycleState::Hibernated,
            SessionLifecycleState::AwaitingGate,
            SessionLifecycleState::Idle,
            SessionLifecycleState::Paused,
            SessionLifecycleState::Terminated(TerminatedReason::Completed),
            SessionLifecycleState::Terminated(TerminatedReason::Failed),
            SessionLifecycleState::Terminated(TerminatedReason::Suspended),
        ] {
            assert_eq!(
                SessionLifecycleState::classify_stored(s.as_str()),
                StoredLifecycle::Known(s),
                "{s:?} must classify as Known"
            );
        }
    }

    /// A `terminated:<reason>` from a newer gateway (or a `TerminatedReason`
    /// added after this build) is terminal on the prefix alone. `FromStr`'s
    /// `_ => Err` arm absorbs such a value silently — adding a reason is a
    /// compile error in `as_str`, but *not* there — so a bare `parse().ok()`
    /// on the read path would read it as alive: children never orphaned, the
    /// workflow never released. That is the #1056 livelock.
    #[test]
    fn classify_stored_treats_an_unknown_terminated_reason_as_terminal() {
        let classified = SessionLifecycleState::classify_stored("terminated:cancelled");
        assert_eq!(classified, StoredLifecycle::UnknownTerminal);
        assert!(classified.is_terminal(), "an unknown reason is still an end");
        assert!(
            classified.permits_workflow_completion(),
            "a terminated root must release its workflow whatever ended it"
        );
        // The strict parse this replaces is what made the guard necessary.
        assert!("terminated:cancelled".parse::<SessionLifecycleState>().is_err());
    }

    /// A value that is neither known nor terminal-by-prefix carries no signal,
    /// so it is read as "still alive": children stay protected and the workflow
    /// stays open for an operator to look at.
    #[test]
    fn classify_stored_is_conservative_about_unrecognised_values() {
        for raw in ["bogus", "", "TERMINATED:completed", "terminated"] {
            let classified = SessionLifecycleState::classify_stored(raw);
            assert_eq!(
                classified,
                StoredLifecycle::Unrecognised,
                "{raw:?} must not be recognised"
            );
            assert!(!classified.is_terminal(), "{raw:?} must not orphan children");
            assert!(
                !classified.permits_workflow_completion(),
                "{raw:?} must not release a workflow"
            );
        }
    }
}
