use clap::{Args, Parser, Subcommand};
use std::path::{Path, PathBuf};

use autonoetic_gateway::llm::{CompletionRequest, Message};
use autonoetic_types::causal_chain::CausalChainEntry;
use autonoetic_types::config::GatewayConfig;
use std::collections::BTreeMap;

/// Default basename for operator config/workspace under `$HOME` (unless `--config` is set).
pub const DEFAULT_OPERATOR_HOME_SUBDIR: &str = ".autonoetic";

fn parse_key_value(s: &str) -> anyhow::Result<(String, String)> {
    let parts: Vec<&str> = s.splitn(2, '=').collect();
    if parts.len() != 2 {
        return Err(anyhow::anyhow!("Invalid KEY=VALUE format: {}", s));
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}

// Re-exports for modules
pub use autonoetic_mcp::{
    AgentExecutor as McpAgentExecutor, McpClient, McpServer, McpTool, McpTransportConfig,
};

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseValidationMode {
    On,
    Off,
    Repair,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum CliApprovalLevel {
    Operator,
    Admin,
}

impl CliApprovalLevel {
    pub fn to_runtime(self) -> autonoetic_types::background::ApprovalLevel {
        match self {
            Self::Operator => autonoetic_types::background::ApprovalLevel::Operator,
            Self::Admin => autonoetic_types::background::ApprovalLevel::Admin,
        }
    }
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum CliGrantScope {
    Root,
    Session,
}

impl CliGrantScope {
    pub fn to_runtime(self) -> autonoetic_types::background::GrantScope {
        match self {
            Self::Root => autonoetic_types::background::GrantScope::RootSession,
            Self::Session => autonoetic_types::background::GrantScope::Session,
        }
    }
}

pub fn parse_grant_target_spec(spec: &str) -> anyhow::Result<autonoetic_types::background::GrantTarget> {
    use autonoetic_types::background::GrantTarget;
    if let Some(val) = spec.strip_prefix("host:") {
        if let Some((h, port_str)) = val.rsplit_once(':') {
            if let Ok(port) = port_str.parse::<u16>() {
                return Ok(GrantTarget::HostAndPort { host: h.to_ascii_lowercase(), port });
            }
        }
        Ok(GrantTarget::ExactHost(val.to_ascii_lowercase()))
    } else if let Some(val) = spec.strip_prefix("suffix:") {
        Ok(GrantTarget::HostSuffix(val.to_ascii_lowercase()))
    } else if let Some(val) = spec.strip_prefix("hostport:") {
        let (host, port_str) = val.rsplit_once(':')
            .ok_or_else(|| anyhow::anyhow!("hostport spec must be 'hostport:host:port', got: {}", spec))?;
        let port: u16 = port_str.parse()
            .map_err(|_| anyhow::anyhow!("invalid port in hostport spec: {}", port_str))?;
        Ok(GrantTarget::HostAndPort { host: host.to_ascii_lowercase(), port })
    } else if let Some(val) = spec.strip_prefix("url:") {
        Ok(GrantTarget::UrlPrefix(val.to_ascii_lowercase()))
    } else {
        Ok(GrantTarget::ExactHost(spec.to_ascii_lowercase()))
    }
}

pub fn parse_ttl(ttl: &str) -> anyhow::Result<String> {
    let secs = if ttl.ends_with('s') {
        ttl.trim_end_matches('s').parse::<i64>()?
    } else if ttl.ends_with('m') {
        ttl.trim_end_matches('m').parse::<i64>()? * 60
    } else if ttl.ends_with('h') {
        ttl.trim_end_matches('h').parse::<i64>()? * 3600
    } else {
        ttl.parse::<i64>()? * 60
    };
    let expires = chrono::Utc::now() + chrono::Duration::seconds(secs);
    Ok(expires.to_rfc3339())
}

pub fn apply_response_validation_override(
    config: &mut GatewayConfig,
    mode: Option<ResponseValidationMode>,
) {
    match mode {
        Some(ResponseValidationMode::On) => {
            config.response_validation.enabled = true;
            config.response_validation.repair_enabled = false;
        }
        Some(ResponseValidationMode::Off) => {
            config.response_validation.enabled = false;
            config.response_validation.repair_enabled = false;
        }
        Some(ResponseValidationMode::Repair) => {
            config.response_validation.enabled = true;
            config.response_validation.repair_enabled = true;
        }
        None => {}
    }
}

#[derive(Parser)]
#[command(
    name = "autonoetic",
    about = "CLI for managing the Autonoetic Agent System",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Path to a custom config.yaml or policy.yaml (default: ~/.autonoetic/)
    #[arg(global = true, long)]
    pub config: Option<String>,

    /// Overrides the Gateway log level (trace, debug, info, warn, error)
    #[arg(global = true, long)]
    pub log_level: Option<String>,

    /// Disables all prompts (essential for CI/CD)
    #[arg(global = true, long)]
    pub non_interactive: bool,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Bootstrap, start gateway, and open chat in one step
    Run(RunArgs),
    /// Manage the Gateway lifecycle
    Gateway(GatewayArgs),
    /// Manage Autonoetic Agents
    Agent(AgentArgs),
    /// Chat with an agent through gateway JSON-RPC ingress
    Chat(ChatArgs),
    /// Inspect causal chain traces
    Trace(TraceArgs),
    /// Ecosystem and Skills management
    Skill(SkillArgs),
    /// Federation and Cluster management
    Federate(FederateArgs),
    /// MCP Integration management
    Mcp(McpArgs),
    /// Security sentinel — status, findings, and triage
    Security(SecurityArgs),
    /// Recording sessions and fixture set management
    Recording(RecordingArgs),
    /// Evaluate agents against recorded fixture sets
    Eval(EvalArgs),
    /// Post-promotion review status
    Review(ReviewArgs),
}

/// Arguments for the all-in-one `run` command.
#[derive(Args)]
pub struct RunArgs {
    /// Optional target agent ID. Defaults to planner.default.
    pub agent_id: Option<String>,
    /// Stable conversation/session identifier.
    #[arg(long)]
    pub session_id: Option<String>,
    /// Resume the most recent session instead of starting a new one.
    #[arg(long)]
    pub resume: bool,
    /// Re-copy bundled reference agents from the Autonoetic repo and re-bootstrap gateway revisions
    /// (equivalent to `autonoetic agent bootstrap --overwrite`). Use after upgrading the binary or when
    /// reference bundles changed (e.g. new `runtime.lock` or manifest fields).
    #[arg(long)]
    pub overwrite: bool,
    /// Interactively select a new provider/model, update config, patch agent
    /// SKILL.md files, and create new revisions. Old revisions are preserved.
    #[arg(long)]
    pub refresh_models: bool,
}

// ---------------------------------------------------------------------------
// Gateway
// ---------------------------------------------------------------------------

#[derive(Args)]
pub struct GatewayArgs {
    #[command(subcommand)]
    pub command: GatewayCommands,
}

#[derive(Subcommand)]
pub enum GatewayCommands {
    /// Starts the Gateway daemon in the foreground
    Start {
        /// Run in the background
        #[arg(short, long)]
        daemon: bool,
        /// Override the default HTTP/TCP ports
        #[arg(long)]
        port: Option<u16>,
        /// Force TLS wrapping on the OFP federation port
        #[arg(long)]
        tls: bool,
        /// Override gateway response validation mode for this daemon run.
        #[arg(long, value_enum)]
        response_validation: Option<ResponseValidationMode>,
    },
    /// Gracefully terminates a background Gateway daemon
    Stop,
    /// Outputs a table of Gateway health, loaded policies, etc.
    Status {
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Inspect or decide pending background approvals.
    Approvals {
        #[command(subcommand)]
        command: GatewayApprovalCommands,
    },
    /// Manage session approval grants (revoke, list).
    Grants {
        #[command(subcommand)]
        command: GatewayGrantCommands,
    },
    /// Inspect or answer pending user interactions.
    Interactions {
        #[command(subcommand)]
        command: GatewayInteractionCommands,
    },
    /// Manage system agents (declared in config, auto-scheduled on startup).
    SystemAgents {
        #[command(subcommand)]
        command: SystemAgentCommands,
    },
    /// Manage agent-submitted constitutional amendment proposals (R+++1).
    Constitution {
        #[command(subcommand)]
        command: GatewayConstitutionCommands,
    },
}

#[derive(Subcommand)]
pub enum GatewayConstitutionCommands {
    /// List amendment proposals.
    Proposals {
        #[command(subcommand)]
        command: GatewayConstitutionProposalCommands,
    },
    /// Apply a release tag to all approved-but-unpublished proposals.
    /// The constitution markdown is *not* edited automatically — the
    /// operator updates the configured constitution source file by hand and the
    /// digest bumps on rebuild.
    Release {
        /// Release tag to record (e.g. `2026-Q2`).
        tag: String,
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum GatewayConstitutionProposalCommands {
    /// List submitted proposals (most recent first).
    List {
        /// Filter by status (`pending`, `under_review`, `approved`, `rejected`, `deferred`).
        #[arg(long)]
        status: Option<String>,
        /// Filter by proposing agent ID.
        #[arg(long)]
        proposer: Option<String>,
        /// Maximum number of results to return.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Show one proposal in full.
    Show {
        /// Proposal identifier.
        proposal_id: String,
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Approve a proposal — queues it for the next release.
    Approve {
        /// Proposal identifier.
        proposal_id: String,
        /// Optional approval note.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Reject a proposal.
    Reject {
        /// Proposal identifier.
        proposal_id: String,
        /// Optional rejection note.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Defer a proposal — keeps it in the queue without a final decision.
    Defer {
        /// Proposal identifier.
        proposal_id: String,
        /// Optional reason.
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum GatewayApprovalCommands {
    /// List pending approval requests.
    List {
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Approve one pending request.
    Approve {
        /// Approval request identifier.
        request_id: String,
        /// Optional approval note.
        #[arg(long)]
        reason: Option<String>,
        /// Secret values to provide for credential prompts (KEY=VALUE format).
        #[arg(long = "secret", value_parser = parse_key_value)]
        secrets: Vec<(String, String)>,
        /// Approver level used to authorize this decision.
        #[arg(long = "approval-level", value_enum, default_value_t = CliApprovalLevel::Operator)]
        approval_level: CliApprovalLevel,
        /// Grant scope: `root` (default) shares with all children/siblings;
        /// `session` limits the grant to the specific child session.
        #[arg(long, value_enum, default_value_t = CliGrantScope::Root)]
        scope: CliGrantScope,
        /// Narrow the grant to specific targets (repeatable).
        /// Syntax: `host:api.github.com`, `suffix:*.github.com`,
        /// `hostport:api.github.com:443`, `url:https://api.github.com/public/`
        #[arg(long = "target", value_name = "SPEC")]
        targets: Vec<String>,
        /// Time-to-live for the grant (e.g. `10m`, `1h`, `30s`).
        #[arg(long)]
        ttl: Option<String>,
        /// Absolute expiry timestamp (RFC3339).
        #[arg(long)]
        until: Option<String>,
        /// Acknowledge a capability that this approval grants (R++2).
        /// Required for `RevisionPromote` approvals — must name every
        /// added/broadened capability type. Repeatable.
        #[arg(long = "acknowledge-capability", value_name = "TYPE")]
        acknowledge_capabilities: Vec<String>,
        /// Confirmation phrase for destructive approval classes (R++4).
        /// Required when the approval has a `confirm_phrase` field set
        /// (e.g. RevisionPromote, CredentialPrompt). Case-insensitive match.
        #[arg(long = "confirm-phrase")]
        confirm_phrase: Option<String>,
    },
    /// Reject one pending request.
    Reject {
        /// Approval request identifier.
        request_id: String,
        /// Optional rejection note.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Interactive TUI to review, approve, or reject pending requests.
    Interactive {
        /// Approver level used when approving from the TUI.
        #[arg(long = "approval-level", value_enum, default_value_t = CliApprovalLevel::Operator)]
        approval_level: CliApprovalLevel,
    },
    /// Show details of a specific approval request.
    Show {
        /// Approval request identifier.
        request_id: String,
    },
    /// Ask a natural-language question about a specific approval request.
    ///
    /// Answers questions like "what URL will it access?", "what code will run?",
    /// "why does this need approval?", "what dependencies does it install?", etc.
    ///
    /// Note: this uses the configured LLM to summarise stored approval fields.
    /// It does *not* address the agent that requested the approval — for that
    /// use `ask-agent` (Phase 5 enrichment, #172).
    Ask {
        /// Approval request identifier.
        request_id: String,
        /// The question to ask about this approval (e.g. "what URL?", "show me the code").
        question: String,
    },
    /// Append an operator note to the approval's enrichment thread (Phase 5, #172).
    ///
    /// Notes are visible to the agent (via `approval.status.enrichment_messages`)
    /// and surfaced in `gateway approvals show`, the interactive TUI, and the
    /// chat approval cards.
    Comment {
        /// Approval request identifier.
        request_id: String,
        /// The note to append (will be redacted before storage).
        message: String,
    },
    /// Ask the agent that requested the approval a clarifying question
    /// (Phase 5, #172).
    ///
    /// Spawns a read-only clarification child session of the same agent,
    /// primed with the parent's digest and approval context, and captures
    /// the reply as a `gate_message`. The parent session is untouched.
    ///
    /// This is distinct from `ask`, which is an ephemeral LLM Q&A on the
    /// approval JSON only — `ask-agent` actually asks the agent.
    AskAgent {
        /// Approval request identifier.
        request_id: String,
        /// The question to ask the agent.
        question: String,
    },
    /// Show approval statistics and analytics.
    Stats {
        /// Filter by agent ID.
        #[arg(long)]
        agent: Option<String>,
        /// Filter by root session ID.
        #[arg(long)]
        session: Option<String>,
        /// Only include approvals since this time (e.g. `1h`, `24h`, `7d`).
        #[arg(long)]
        since: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum GatewayGrantCommands {
    /// List active grants for a root session.
    List {
        /// Root session ID.
        root_session: String,
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Revoke one or all grants for a root session.
    Revoke {
        /// Root session ID.
        root_session: String,
        /// Revoke grants for a specific host only.
        #[arg(long, conflicts_with = "all")]
        host: Option<String>,
        /// Revoke all grants for the session.
        #[arg(long)]
        all: bool,
        /// Reason for revocation (recorded in audit trail).
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum GatewayInteractionCommands {
    /// List pending user interactions.
    List {
        /// Filter by root session ID.
        #[arg(long)]
        root_session: Option<String>,
        /// Filter by session ID.
        #[arg(long)]
        session: Option<String>,
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Answer a pending user interaction.
    Answer {
        /// Interaction identifier.
        interaction_id: String,
        /// Answer text (for freeform answers).
        #[arg(long)]
        text: Option<String>,
        /// Answer option ID (for structured choices).
        #[arg(long)]
        option: Option<String>,
    },
    /// Cancel a pending user interaction.
    Cancel {
        /// Interaction identifier.
        interaction_id: String,
        /// Optional cancellation reason.
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum SystemAgentCommands {
    /// List declared system agents and their status.
    List {
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Reconcile system agents now (create missing cron jobs).
    Bootstrap,
    /// Manually trigger a system agent run (bypasses schedule).
    Run {
        /// Agent ID to trigger.
        agent_id: String,
    },
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

#[derive(Args)]
pub struct AgentArgs {
    #[command(subcommand)]
    pub command: AgentCommands,
}

#[derive(Subcommand)]
pub enum AgentCommands {
    /// Scaffolds a new Autonoetic Agent directory
    Init {
        /// Agent ID to create
        agent_id: String,
        /// Template to use (e.g., researcher, coder, auditor)
        #[arg(long)]
        template: Option<String>,
        /// LLM preset name from config (e.g., agentic, coding, fast)
        #[arg(long)]
        preset: Option<String>,
        /// LLM provider override (e.g., openai, anthropic, gemini, openrouter)
        #[arg(long)]
        provider: Option<String>,
        /// LLM model override (e.g., gpt-4o, claude-sonnet-4-20250514)
        #[arg(long)]
        model: Option<String>,
    },
    /// Boots an Agent and connects it to the Gateway
    Run {
        /// Agent ID to run
        agent_id: String,
        /// Initial message kickoff
        message: Option<String>,
        /// Drops the user into a persistent chat loop
        #[arg(short, long)]
        interactive: bool,
        /// Boots the agent headless
        #[arg(long)]
        headless: bool,
        /// Override response validation mode for this local run.
        #[arg(long, value_enum)]
        response_validation: Option<ResponseValidationMode>,
        /// Record all HTTP traffic as fixtures (Recording mode).
        #[arg(long)]
        record_network: bool,
        /// Max recording duration in seconds (default: 600).
        #[arg(long)]
        recording_duration: Option<u64>,
        /// Max requests to capture (default: 1000).
        #[arg(long)]
        recording_max_requests: Option<u64>,
        /// Max total fixture bytes (default: 50MB).
        #[arg(long)]
        recording_max_bytes: Option<u64>,
    },
    /// Lists all local Agents registered with the Gateway
    List,
    /// Bootstraps runtime agents from reference bundles
    Bootstrap {
        /// Optional path to reference bundles root (defaults to auto-detection)
        #[arg(long)]
        from: Option<String>,
        /// Overwrite existing target agent directories
        #[arg(long)]
        overwrite: bool,
        /// Interactively select a new provider/model, update config, patch agent
        /// SKILL.md files, and create new revisions. Old revisions are preserved.
        #[arg(long)]
        refresh_models: bool,
    },
    /// Inspect mutable alias bindings and active revisions
    Alias {
        #[command(subcommand)]
        command: AgentAliasCommands,
    },
    /// Deterministically seed alias activation to a specific revision
    Seed {
        /// Logical agent ID / alias ID to activate
        agent_id: String,
        /// Target immutable revision ID
        revision_id: String,
        /// Optional explicit promotion record ID (useful for deterministic tests)
        #[arg(long)]
        promotion_id: Option<String>,
        /// Optional reason attached to promotion history
        #[arg(long)]
        reason: Option<String>,
        /// Emit machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Revision-based lifecycle operations (create/promote)
    Revision {
        #[command(subcommand)]
        command: AgentRevisionCommands,
    },
    /// Inspect durable promote/rollback history
    PromotionHistory {
        /// Filter by logical agent ID
        #[arg(long)]
        agent_id: Option<String>,
        /// Emit machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Shows available LLM presets and template mappings
    Presets,
    /// Creates a default config.yaml with LLM presets
    InitConfig {
        /// Output path for config.yaml (default: ./config.yaml)
        #[arg(long)]
        output: Option<String>,
        /// Overwrite existing config file
        #[arg(long)]
        overwrite: bool,
    },
    /// Imports an external AgentSkills.io skill into Autonoetic
    ImportSkill {
        /// Path to the external skill directory (must contain SKILL.md)
        #[arg(long)]
        from: String,
        /// Agent ID to import as (e.g., myagent.default)
        #[arg(long)]
        agent_id: String,
        /// Trust mode: generous (auto-grant), strict (approval per capability), audit (dry-run)
        #[arg(long, value_enum, default_value = "strict")]
        trust: TrustMode,
        /// LLM provider for the imported agent (default: openai)
        #[arg(long)]
        provider: Option<String>,
        /// LLM model for the imported agent (default: gpt-4o)
        #[arg(long)]
        model: Option<String>,
    },
    /// Manage vault-stored credentials
    Credential {
        #[command(subcommand)]
        command: AgentCredentialCommands,
    },
}

/// Trust mode for importing external AgentSkills.io skills.
#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
pub enum TrustMode {
    /// Auto-grant all capabilities inferred from allowed-tools. No approval gates.
    Generous,
    /// Preserve inferred capabilities but require approval for all privileged operations (default).
    #[default]
    Strict,
    /// Restrict to read-only access; all operations require approval before execution.
    Audit,
}

#[derive(Subcommand)]
pub enum AgentCredentialCommands {
    /// Store a secret in the encrypted vault and register a credential record
    Put {
        /// Service name (e.g., openweathermap, github, stripe)
        #[arg(long)]
        service: String,
        /// Vault key name for the secret (e.g., OPENWEATHER_API_KEY)
        #[arg(long)]
        secret_name: String,
        /// Read the secret value from this environment variable instead of prompting
        #[arg(long, conflicts_with = "value")]
        from_env: Option<String>,
        /// Provide the secret value directly (use --from-env for better security)
        #[arg(long)]
        value: Option<String>,
        /// Credential ID (auto-generated if omitted)
        #[arg(long)]
        credential_id: Option<String>,
        /// How the credential is injected when used (e.g., env:API_KEY, bearer, header:X-Custom)
        #[arg(long)]
        inject_as: Option<String>,
        /// Hosts this credential is allowed to be used with (e.g., api.openweathermap.org)
        #[arg(long)]
        allowed_hosts: Option<Vec<String>>,
        /// ISO 8601 expiry timestamp
        #[arg(long)]
        expires_at: Option<String>,
    },
    /// List registered credentials (shows metadata only, never secret values)
    List {
        /// Filter by service name
        #[arg(long)]
        service: Option<String>,
        /// Emit machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Remove a credential and its secret from the vault
    Rm {
        /// Credential ID to remove
        credential_id: String,
    },
}

#[derive(Subcommand)]
pub enum AgentRevisionCommands {
    /// Create immutable revision from an AgentBundle artifact
    Create {
        /// Logical agent ID to create the revision for
        agent_id: String,
        /// Source artifact ID (must be kind=agent_bundle)
        artifact_id: String,
        /// Optional base revision ID/ref for lineage metadata
        #[arg(long)]
        base_revision_id: Option<String>,
        /// Optional short summary
        #[arg(long)]
        summary: Option<String>,
        /// Emit machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Promote a revision to active alias target
    Promote {
        /// Logical agent ID / alias ID
        agent_id: String,
        /// Target revision ID
        revision_id: String,
        /// Optional reason in promotion history
        #[arg(long)]
        reason: Option<String>,
        /// Optional eval run that must match the target revision
        #[arg(long)]
        required_eval_run_id: Option<String>,
        /// Emit machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum AgentAliasCommands {
    /// List aliases and their active revision targets
    List {
        /// Filter by agent_id or alias_id (MVP: same value)
        #[arg(long)]
        agent_id: Option<String>,
        /// Emit machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Inspect one alias target in detail
    Inspect {
        /// Alias ID to inspect (MVP default alias is agent_id)
        alias_id: String,
        /// Emit machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
}

// ---------------------------------------------------------------------------
// Chat
// ---------------------------------------------------------------------------

#[derive(Args)]
pub struct ChatArgs {
    /// Optional target agent ID. If omitted, gateway ingress resolves to session/default lead agent.
    pub agent_id: Option<String>,
    /// Stable sender identity for the terminal client.
    #[arg(long)]
    pub sender_id: Option<String>,
    /// Stable channel identity for the terminal surface.
    #[arg(long)]
    pub channel_id: Option<String>,
    /// Stable conversation/session identifier.
    #[arg(long)]
    pub session_id: Option<String>,
    /// Resume the most recent session instead of starting a new one.
    #[arg(long)]
    pub resume: bool,
    /// Suppress prompts and banners for deterministic scripted tests.
    #[arg(long)]
    pub test_mode: bool,
}

// ---------------------------------------------------------------------------
// Trace
// ---------------------------------------------------------------------------

#[derive(Args)]
pub struct TraceArgs {
    #[command(subcommand)]
    pub command: TraceCommands,
}

#[derive(Subcommand)]
pub enum TraceCommands {
    /// List known sessions across agent traces
    Sessions {
        /// Restrict lookup to one agent
        #[arg(long)]
        agent: Option<String>,
        /// Emit machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Show all events for one session
    Show {
        /// Session identifier
        session_id: String,
        /// Restrict lookup to one agent
        #[arg(long)]
        agent: Option<String>,
        /// Emit machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Show one specific event by log_id
    Event {
        /// Event/log identifier
        log_id: String,
        /// Restrict lookup to one agent
        #[arg(long)]
        agent: Option<String>,
        /// Emit machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Rebuild unified session timeline from gateway + agent causal logs
    Rebuild {
        /// Session identifier to rebuild
        session_id: String,
        /// Restrict lookup to one agent
        #[arg(long)]
        agent: Option<String>,
        /// Emit machine-readable JSON output
        #[arg(long)]
        json: bool,
        /// Skip integrity checks
        #[arg(long)]
        skip_checks: bool,
    },
    /// Follow session events in real-time as they happen
    Follow {
        /// Session identifier to follow
        session_id: String,
        /// Restrict to one agent
        #[arg(long)]
        agent: Option<String>,
        /// Emit machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Fork a session from a snapshot to explore alternative paths
    Fork {
        /// Source session ID to fork from
        session_id: String,
        /// Branch message to append (e.g., "Try a different approach")
        #[arg(long)]
        message: Option<String>,
        /// New session ID (auto-generated if not provided)
        #[arg(long)]
        new_session_id: Option<String>,
        /// Fork from specific turn number (default: latest)
        #[arg(long)]
        at_turn: Option<usize>,
        /// Target agent ID (defaults to source agent)
        #[arg(long)]
        agent: Option<String>,
        /// Start interactive chat after forking
        #[arg(long)]
        interactive: bool,
        /// Emit machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Show conversation history for a session
    History {
        /// Session identifier
        session_id: String,
        /// Restrict lookup to one agent
        #[arg(long)]
        agent: Option<String>,
        /// Emit machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Print the post-session narrative for a root session (`post_session_narrative.md`)
    Digest {
        /// Session identifier (root or nested; root segment selects storage)
        session_id: String,
        /// Emit machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Show durable workflow orchestration events (gateway workflow store)
    Workflow {
        /// Workflow id (`wf-…`) or root session id when `--root` is set
        workflow_or_root: String,
        /// Treat `workflow_or_root` as root session id and resolve `wf-*` via index
        #[arg(long)]
        root: bool,
        /// Emit machine-readable JSON (pretty document once; with `--follow`, one JSON object per new event line)
        #[arg(long)]
        json: bool,
        /// Poll workflow events from the gateway store and print new lines (Ctrl+C to stop)
        #[arg(long)]
        follow: bool,
    },
    /// Show a text workflow graph from the durable store (root session id or `wf-…`)
    Graph {
        /// Root session id (same tree as `agent.spawn` root) or a `wf-…` workflow id
        session_or_workflow: String,
        /// Emit machine-readable JSON (with `--follow`, one minified snapshot per poll line)
        #[arg(long)]
        json: bool,
        /// Refresh the graph every second (clears the terminal when not using `--json`)
        #[arg(long)]
        follow: bool,
    },
}

// ---------------------------------------------------------------------------
// Skill
// ---------------------------------------------------------------------------

#[derive(Args)]
pub struct SkillArgs {
    #[command(subcommand)]
    pub command: SkillCommands,
}

#[derive(Subcommand)]
pub enum SkillCommands {
    /// Downloads and installs an AgentSkills.io compliant bundle
    Install {
        /// GitHub URL or Skill ID
        url_or_id: String,
        /// Target agent ID
        #[arg(long)]
        agent: Option<String>,
    },
    /// Removes a skill from an Agent's capability list
    Uninstall {
        /// Name of the skill to uninstall
        skill_name: String,
        /// Target agent ID
        #[arg(long)]
        agent: String,
    },
}

// ---------------------------------------------------------------------------
// Federate
// ---------------------------------------------------------------------------

#[derive(Args)]
pub struct FederateArgs {
    #[command(subcommand)]
    pub command: FederateCommands,
}

#[derive(Subcommand)]
pub enum FederateCommands {
    /// Connects the local Gateway to a remote peer via OFP
    Join {
        /// Remote peer address
        peer_address: String,
    },
    /// Outputs the local PeerRegistry
    List,
}

// ---------------------------------------------------------------------------
// MCP
// ---------------------------------------------------------------------------

#[derive(Args)]
pub struct McpArgs {
    #[command(subcommand)]
    pub command: McpCommands,
}

#[derive(Subcommand)]
pub enum McpCommands {
    /// Registers a local MCP server with the Gateway
    Add {
        /// MCP Server name
        server_name: String,
        /// Subprocess command (stdio transport).
        #[arg(long)]
        command: Option<String>,
        /// Optional SSE endpoint transport URL.
        #[arg(long)]
        sse_url: Option<String>,
        /// Optional command arguments
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Temporarily runs the Gateway as an MCP Server on stdio
    Expose {
        /// Agent ID to expose
        agent_id: String,
    },
}

// ===========================================================================
// Shared Utilities
// ===========================================================================

pub fn dirs_or_default() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(DEFAULT_OPERATOR_HOME_SUBDIR))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_OPERATOR_HOME_SUBDIR))
}

pub fn mcp_registry_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .map(|p| p.join("mcp_servers.json"))
        .unwrap_or_else(|| PathBuf::from("mcp_servers.json"))
}

pub fn load_mcp_servers(path: &Path) -> anyhow::Result<Vec<McpServer>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let raw = std::fs::read_to_string(path)?;
    let servers = serde_json::from_str::<Vec<McpServer>>(&raw)?;
    Ok(servers)
}

pub fn save_mcp_servers(path: &Path, servers: &[McpServer]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(servers)?;
    std::fs::write(path, body)?;
    Ok(())
}

pub fn default_terminal_sender_id() -> String {
    std::env::var("USER")
        .ok()
        .or_else(|| std::env::var("USERNAME").ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "terminal-user".to_string())
}

pub fn default_terminal_channel_id(sender_id: &str, target_hint: &str) -> String {
    format!("terminal:{}:{}", sender_id, target_hint)
}

/// Terminal sessions label metadata as `channel.kind = "terminal"`.
///
/// Remote transports (HTTP bridges, Discord bots, etc.) should use their own `kind`
/// strings (`"http"`, `"discord"`, `"whatsapp"`, …) following the envelope conventions
/// documented in `docs/remote-agents-http-api.md`.
pub fn terminal_channel_envelope(
    channel_id: &str,
    sender_id: &str,
    session_id: &str,
) -> serde_json::Value {
    serde_json::json!({
        "channel": {
            "kind": "terminal",
            "channel_id": channel_id,
            "sender_id": sender_id,
            "session_id": session_id
        }
    })
}

#[derive(Debug)]
pub struct AgentTrace {
    pub agent_id: String,
    pub entries: Vec<CausalChainEntry>,
}

#[derive(Debug)]
pub struct TraceEntry {
    pub agent_id: String,
    pub entry: CausalChainEntry,
}

#[derive(Debug)]
pub struct SessionSummary {
    pub agent_id: String,
    pub session_id: String,
    pub first_timestamp: String,
    pub last_timestamp: String,
    pub event_count: usize,
    pub max_event_seq: u64,
}

pub fn collect_session_summaries(traces: &[AgentTrace]) -> Vec<SessionSummary> {
    let mut by_session: BTreeMap<(String, String), SessionSummary> = BTreeMap::new();
    for trace in traces {
        for entry in &trace.entries {
            let key = (trace.agent_id.clone(), entry.session_id.clone());
            let summary = by_session.entry(key).or_insert_with(|| SessionSummary {
                agent_id: trace.agent_id.clone(),
                session_id: entry.session_id.clone(),
                first_timestamp: entry.timestamp.clone(),
                last_timestamp: entry.timestamp.clone(),
                event_count: 0,
                max_event_seq: entry.event_seq,
            });
            summary.event_count += 1;
            if entry.timestamp < summary.first_timestamp {
                summary.first_timestamp = entry.timestamp.clone();
            }
            if entry.timestamp > summary.last_timestamp {
                summary.last_timestamp = entry.timestamp.clone();
            }
            if entry.event_seq > summary.max_event_seq {
                summary.max_event_seq = entry.event_seq;
            }
        }
    }

    by_session.into_values().collect::<Vec<_>>()
}

pub struct CliAgentExecutor {
    pub agents_dir: PathBuf,
    pub client: reqwest::Client,
}

#[async_trait::async_trait]
impl McpAgentExecutor for CliAgentExecutor {
    async fn call_agent(&self, agent_id: &str, message: &str) -> anyhow::Result<String> {
        let repo = autonoetic_gateway::AgentRepository::new(self.agents_dir.clone());
        let loaded = repo.get(agent_id).await?;
        let llm_config = loaded
            .manifest
            .llm_config
            .ok_or_else(|| anyhow::anyhow!("Agent '{}' is missing llm_config", agent_id))?;

        let driver =
            autonoetic_gateway::llm::build_driver(llm_config.clone(), self.client.clone())?;
        let req = CompletionRequest::simple(
            llm_config.model,
            vec![Message::system(loaded.instructions), Message::user(message)],
        );
        let resp = driver.complete(&req).await?;
        if resp.text.trim().is_empty() {
            anyhow::bail!("Agent '{}' returned an empty response", agent_id);
        }
        Ok(resp.text)
    }
}

pub struct ActivatedMcpServer {
    pub name: String,
    pub tools: Vec<autonoetic_mcp::McpTool>,
    pub _client: McpClient,
}

pub struct McpRuntime {
    pub servers: Vec<ActivatedMcpServer>,
}

impl McpRuntime {
    pub fn empty() -> Self {
        Self { servers: vec![] }
    }

    pub fn summary_lines(&self) -> Vec<String> {
        if self.servers.is_empty() {
            return vec!["MCP activation: no registered MCP servers.".to_string()];
        }

        let mut lines = vec![format!(
            "MCP activation: {} server(s) active, {} tool(s) total.",
            self.servers.len(),
            self.servers.iter().map(|s| s.tools.len()).sum::<usize>()
        )];
        for server in &self.servers {
            lines.push(format!(
                "  MCP server '{}' => {} tool(s)",
                server.name,
                server.tools.len()
            ));
            for tool in &server.tools {
                lines.push(format!("    - {}", tool.name));
            }
        }
        lines
    }
}

pub async fn activate_registered_mcp_servers(config_path: &Path) -> anyhow::Result<McpRuntime> {
    let registry_path = mcp_registry_path(config_path);
    let servers = load_mcp_servers(&registry_path)?;
    if servers.is_empty() {
        return Ok(McpRuntime::empty());
    }

    let mut activated = Vec::with_capacity(servers.len());
    for server in servers {
        let mut client = McpClient::connect(&server).await?;
        let tools = client.list_tools().await?;
        activated.push(ActivatedMcpServer {
            name: server.name,
            tools,
            _client: client,
        });
    }

    Ok(McpRuntime { servers: activated })
}

// ---------------------------------------------------------------------------
// Security
// ---------------------------------------------------------------------------

#[derive(Args)]
pub struct SecurityArgs {
    #[command(subcommand)]
    pub command: SecurityCommands,
}

#[derive(Subcommand)]
pub enum SecurityCommands {
    /// Show sentinel health: finding counts, triage backlog, last sweep time.
    Status {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// List security findings with optional filters.
    Findings {
        /// Filter by severity (critical, warning, info).
        #[arg(long)]
        severity: Option<String>,

        /// Filter by finding type (e.g. credential_leak, sandbox_escape_attempt).
        #[arg(long, name = "type")]
        finding_type: Option<String>,

        /// Filter by triage state (pending, true_positive, false_positive, benign, deferred).
        #[arg(long)]
        triage: Option<String>,

        /// Maximum number of findings to show (default: 50).
        #[arg(long, default_value = "50")]
        limit: u32,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Mark a single finding with a triage decision.
    ///
    /// Example: autonoetic security triage sec_abc123 false_positive --reason "CI test pattern"
    Triage {
        /// Finding ID to triage.
        finding_id: String,

        /// Triage state: pending, true_positive, false_positive, benign, deferred.
        state: String,

        /// Short reason for the decision.
        /// Required when state is not 'pending'; validated at runtime.
        #[arg(long)]
        reason: Option<String>,
    },

    /// Bulk-triage all pending findings matching a filter.
    ///
    /// Example: autonoetic security bulk-triage false_positive \
    ///   --reason "internal CI" --type credential_leak --dry-run
    BulkTriage {
        /// Triage state to apply: true_positive, false_positive, benign, deferred.
        /// 'pending' is not accepted; use individual triage to reset a single finding.
        state: String,

        /// Reason for the bulk decision (required).
        #[arg(long)]
        reason: String,

        /// Restrict to findings of this severity.
        #[arg(long)]
        severity: Option<String>,

        /// Restrict to findings of this type.
        #[arg(long, name = "type")]
        finding_type: Option<String>,

        /// Print matching findings without updating them.
        #[arg(long)]
        dry_run: bool,
    },

    /// List red-team attack-pattern proposals.
    Patterns {
        /// Filter by status: pending, accepted, rejected.
        #[arg(long)]
        status: Option<String>,

        /// Maximum number of proposals to show (default: 50).
        #[arg(long, default_value = "50")]
        limit: u32,

        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Accept a red-team attack-pattern proposal.
    ///
    /// Example: autonoetic security pattern-accept pattern-abc123 \
    ///   --type phase1 --notes "Clear regex, confirmed deterministic"
    PatternAccept {
        /// Pattern proposal ID.
        pattern_id: String,

        /// Target check layer: phase1 (deterministic) or phase2 (llm-judgment).
        #[arg(long, name = "type")]
        check_type: String,

        /// Optional operator notes.
        #[arg(long)]
        notes: Option<String>,
    },

    /// Reject a red-team attack-pattern proposal.
    ///
    /// Example: autonoetic security pattern-reject pattern-abc123 \
    ///   --notes "Already covered by existing credential_leak check"
    PatternReject {
        /// Pattern proposal ID.
        pattern_id: String,

        /// Optional operator notes.
        #[arg(long)]
        notes: Option<String>,
    },
}

/// Arguments for `autonoetic recording` subcommand.
#[derive(Args)]
pub struct RecordingArgs {
    #[command(subcommand)]
    pub command: RecordingCommands,
}

#[derive(Subcommand)]
pub enum RecordingCommands {
    /// List recording sessions.
    List {
        /// Filter by agent ID.
        #[arg(long)]
        agent: Option<String>,
        /// Maximum results (default: 20).
        #[arg(long, default_value = "20")]
        limit: i64,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Inspect a recording session.
    Inspect {
        /// Recording session ID (rs_...).
        session_id: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Delete a recording session and its fixture set.
    Delete {
        /// Recording session ID (rs_...).
        session_id: String,
    },
    /// Cancel a running recording session.
    Cancel {
        /// Recording session ID (rs_...).
        session_id: String,
    },
}

/// Arguments for `autonoetic eval` subcommand.
#[derive(Args)]
pub struct EvalArgs {
    #[command(subcommand)]
    pub command: EvalCommands,
}

#[derive(Subcommand)]
pub enum EvalCommands {
    /// Run sealed evaluation against a recorded fixture set.
    ///
    /// Pre-populates the artifact's fixture directory from a recorded
    /// fixture set, then spawns the sealed evaluator to run
    /// deterministically against the recorded traffic.
    Sealed {
        /// Artifact ref to evaluate (ar_xxxxxxxx).
        #[arg(long)]
        artifact_ref: String,
        /// Fixture set ID to replay (fs_xxxxxxxx).
        #[arg(long)]
        fixture_set: String,
        /// Evaluator agent ID (default: sealed_evaluator.default).
        #[arg(long, default_value = "sealed_evaluator.default")]
        agent_id: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
        /// Max evaluation duration in seconds.
        #[arg(long, default_value = "300")]
        timeout: u64,
    },
}

/// Arguments for `autonoetic review` subcommand.
#[derive(Args)]
pub struct ReviewArgs {
    #[command(subcommand)]
    pub command: ReviewCommands,
}

#[derive(Subcommand)]
pub enum ReviewCommands {
    /// Show post-promotion review status for all agents or a specific agent.
    Status {
        /// Filter by agent ID.
        #[arg(long)]
        agent: Option<String>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Inspect a specific post-promotion review.
    Inspect {
        /// Review ID.
        review_id: String,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show post-promotion review history.
    History {
        /// Filter by agent ID.
        #[arg(long)]
        agent: Option<String>,
        /// Maximum results (default: 20).
        #[arg(long, default_value = "20")]
        limit: i64,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collect_session_summaries_groups_sessions() {
        let traces = vec![AgentTrace {
            agent_id: "agent_demo".to_string(),
            entries: vec![
                CausalChainEntry {
                    timestamp: "2026-03-06T10:00:00Z".to_string(),
                    log_id: "l1".to_string(),
                    actor_id: "agent_demo".to_string(),
                    session_id: "s1".to_string(),
                    turn_id: Some("turn-000001".to_string()),
                    event_seq: 1,
                    category: "session".to_string(),
                    action: "start".to_string(),
                    target: None,
                    status: autonoetic_types::causal_chain::EntryStatus::Success,
                    reason: None,
                    payload: None,
                    payload_hash: None,
                    prev_hash: "genesis".to_string(),
                    entry_hash: "h1".to_string(),
                },
                CausalChainEntry {
                    timestamp: "2026-03-06T10:00:02Z".to_string(),
                    log_id: "l2".to_string(),
                    actor_id: "agent_demo".to_string(),
                    session_id: "s1".to_string(),
                    turn_id: Some("turn-000001".to_string()),
                    event_seq: 2,
                    category: "lifecycle".to_string(),
                    action: "wake".to_string(),
                    target: None,
                    status: autonoetic_types::causal_chain::EntryStatus::Success,
                    reason: None,
                    payload: None,
                    payload_hash: None,
                    prev_hash: "h1".to_string(),
                    entry_hash: "h2".to_string(),
                },
                CausalChainEntry {
                    timestamp: "2026-03-06T10:05:00Z".to_string(),
                    log_id: "l3".to_string(),
                    actor_id: "agent_demo".to_string(),
                    session_id: "s2".to_string(),
                    turn_id: Some("turn-000001".to_string()),
                    event_seq: 1,
                    category: "session".to_string(),
                    action: "start".to_string(),
                    target: None,
                    status: autonoetic_types::causal_chain::EntryStatus::Success,
                    reason: None,
                    payload: None,
                    payload_hash: None,
                    prev_hash: "genesis".to_string(),
                    entry_hash: "h3".to_string(),
                },
            ],
        }];

        let sessions = collect_session_summaries(&traces);
        assert_eq!(sessions.len(), 2, "expected one summary per session");
        let s1 = sessions
            .iter()
            .find(|s| s.session_id == "s1")
            .expect("s1 should be present");
        assert_eq!(s1.event_count, 2);
        assert_eq!(s1.max_event_seq, 2);
    }
}
