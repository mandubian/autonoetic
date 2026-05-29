use crate::llm::ToolDefinition;
use crate::log_redaction::looks_like_secret_value;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::prompt_budget::tool_tier;
use crate::sandbox::{DependencyPlan, DependencyRuntime, SandboxMount};
use autonoetic_types::agent::{AgentManifest, ToolTier};
use autonoetic_types::background::ApprovalRequest;
use autonoetic_types::capability::Capability;
use autonoetic_types::tool_error::tagged;
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use serde_json::{json, Value};

use std::path::Path;

/// A file to be installed as part of an agent.
#[derive(Debug, Deserialize, serde::Serialize, Clone)]
pub struct InstallAgentFile {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Default)]
pub struct ToolMetadata {
    pub path: Option<String>,
}

/// Context for filtering tools by tier based on workflow state.
///
/// Allows progressive disclosure of tools based on the agent's current
/// workflow phase. For example, during approval gates only Core tools
/// may be exposed, while Specialized tools like web search are hidden.
#[derive(Debug, Clone)]
pub struct ToolTierFilter {
    /// Tiers that are allowed. Empty means all tiers are allowed.
    pub allowed_tiers: Vec<ToolTier>,
    /// When true, always include tools needed for approval interactions
    /// regardless of tier (e.g., approval.status, approval.answer).
    pub always_include_approval_tools: bool,
    /// When true, always include inspection tools (observability_*,
    /// knowledge_search/read/search_by_tags, constitution_read, content_read,
    /// execution_search, digest_query) regardless of tier. Used by degraded
    /// sessions so the agent can still diagnose its own state — an agent
    /// that cannot see why it was degraded cannot recover (Ri-0.5 spirit).
    pub always_include_inspection_tools: bool,
    /// When true, override all other rules with a strict read-only allowlist:
    /// only inspection tools (observability/knowledge/constitution/content_read
    /// /execution_search) pass. Used for clarification child sessions
    /// (SessionState::Clarification) so an operator probe via `ask-agent`
    /// structurally cannot trigger any action — even if the agent's manifest
    /// or the system prompt would allow it.
    pub clarification_read_only: bool,
    /// Child sessions normally omit [`ToolTier::Specialized`]. When true,
    /// `promotion_record` still passes [`Self::allows`] so promotion-gate
    /// delegate agents can persist verdicts without widening the tier to all
    /// specialized tools (e.g. `web_search`). Not set during pending-approval
    /// narrowing or degraded mode.
    pub allow_promotion_record_without_specialized_tier: bool,
}

impl ToolTierFilter {
    /// Create a filter that allows only Core tier tools.
    pub fn core_only() -> Self {
        Self {
            allowed_tiers: vec![ToolTier::Core],
            always_include_approval_tools: false,
            always_include_inspection_tools: false,
            clarification_read_only: false,
            allow_promotion_record_without_specialized_tier: false,
        }
    }

    /// Create a filter that allows Core and Workflow tier tools.
    pub fn core_and_workflow() -> Self {
        Self {
            allowed_tiers: vec![ToolTier::Core, ToolTier::Workflow],
            always_include_approval_tools: false,
            always_include_inspection_tools: false,
            clarification_read_only: false,
            allow_promotion_record_without_specialized_tier: false,
        }
    }

    /// Create a filter that allows Core + Workflow tiers and always includes
    /// approval tools. Used when approvals are pending so the agent can still
    /// check approval status but cannot launch new specialized operations.
    pub fn core_and_workflow_with_approvals() -> Self {
        Self {
            allowed_tiers: vec![ToolTier::Core, ToolTier::Workflow],
            always_include_approval_tools: true,
            always_include_inspection_tools: false,
            clarification_read_only: false,
            allow_promotion_record_without_specialized_tier: false,
        }
    }

    /// Create a filter that allows all tiers (no filtering).
    pub fn all() -> Self {
        Self {
            allowed_tiers: vec![],
            always_include_approval_tools: false,
            always_include_inspection_tools: false,
            clarification_read_only: false,
            allow_promotion_record_without_specialized_tier: false,
        }
    }

    /// Filter for `SessionState::Degraded` (R-7.18): Core tier + inspection
    /// tools. The agent has lost specialized capabilities but retains the
    /// ability to read its own causal chain, look up the rule that degraded
    /// it, and inspect what it was doing — so recovery and reporting are
    /// possible.
    pub fn degraded() -> Self {
        Self {
            allowed_tiers: vec![ToolTier::Core],
            always_include_approval_tools: false,
            always_include_inspection_tools: true,
            clarification_read_only: false,
            allow_promotion_record_without_specialized_tier: false,
        }
    }

    /// Filter for `SessionState::Clarification` — read-only by construction.
    /// Whitelist is name-prefix based so it survives manifests that declare
    /// elevated tiers; the constitutional guarantee is that an operator probe
    /// cannot trigger an action.
    pub fn clarification() -> Self {
        Self {
            allowed_tiers: vec![],
            always_include_approval_tools: false,
            always_include_inspection_tools: false,
            clarification_read_only: true,
            allow_promotion_record_without_specialized_tier: false,
        }
    }

    /// True iff `tool_name` is on the clarification read-only allowlist.
    ///
    /// Tools are explicitly enumerated rather than prefix-matched so that a
    /// newly-added action tool sharing a prefix (e.g. `constitution_propose_*`,
    /// `observability_emit_*`) does not silently slip into the allowlist.
    fn is_clarification_safe(tool_name: &str) -> bool {
        matches!(
            tool_name,
            "observability_search"
                | "observability_read"
                | "observability_read_reasoning"
                | "constitution_read"
                | "knowledge_search"
                | "knowledge_read"
                | "knowledge_search_by_tags"
                | "content_read"
                | "execution_search"
                | "digest_query"
        )
    }

    /// Check if a tool with the given name passes this filter.
    /// Derives tier from the tool name prefix.
    /// Also respects always_include_approval_tools for approval-prefixed tools.
    pub fn allows(&self, tool_name: &str) -> bool {
        if self.clarification_read_only {
            return Self::is_clarification_safe(tool_name);
        }
        if self.always_include_inspection_tools && Self::is_clarification_safe(tool_name) {
            return true;
        }
        if self.allowed_tiers.is_empty() {
            return true;
        }
        if self.always_include_approval_tools && tool_name.starts_with("approval_") {
            return true;
        }
        if self.allow_promotion_record_without_specialized_tier && tool_name == "promotion_record" {
            return true;
        }
        self.allows_tier(tool_tier(tool_name))
    }

    /// Check if a tool with the given name and tier passes this filter.
    /// Use this when the tier is already known (e.g. from NativeTool::tier()).
    /// Also respects always_include_approval_tools for approval-prefixed tools.
    pub fn allows_tool(&self, tool_name: &str, tier: ToolTier) -> bool {
        if self.clarification_read_only {
            return Self::is_clarification_safe(tool_name);
        }
        if self.always_include_inspection_tools && Self::is_clarification_safe(tool_name) {
            return true;
        }
        if self.allowed_tiers.is_empty() {
            return true;
        }
        if self.always_include_approval_tools && tool_name.starts_with("approval_") {
            return true;
        }
        if self.allow_promotion_record_without_specialized_tier && tool_name == "promotion_record" {
            return true;
        }
        self.allows_tier(tier)
    }

    /// Check if a tool with the given tier passes this filter.
    /// Note: this does not check always_include_approval_tools — use allows_tool() for that.
    pub fn allows_tier(&self, tier: ToolTier) -> bool {
        if self.allowed_tiers.is_empty() {
            return true;
        }
        self.allowed_tiers.contains(&tier)
    }
}

pub trait NativeTool: Send + Sync {
    fn name(&self) -> &'static str;
    fn definition(&self) -> ToolDefinition;
    fn is_available(&self, manifest: &AgentManifest) -> bool;

    /// The tier of this tool for progressive disclosure.
    /// Defaults to deriving from the tool name prefix.
    fn tier(&self) -> ToolTier {
        tool_tier(self.name())
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String>;

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}

pub struct NativeToolRegistry {
    tools: Vec<Box<dyn NativeTool>>,
}

impl NativeToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn register(&mut self, tool: Box<dyn NativeTool>) {
        self.tools.push(tool);
    }

    pub fn available_definitions(&self, manifest: &AgentManifest) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .filter(|t| t.is_available(manifest))
            .map(|t| with_intent_schema(t.definition()))
            .collect()
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.name() == name)
    }

    /// Collect tool definitions with tier-based filtering based on workflow context.
    /// When filter is None, returns all available tools (same as `available_definitions`).
    pub fn available_definitions_filtered(
        &self,
        manifest: &AgentManifest,
        filter: Option<&ToolTierFilter>,
    ) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .filter(|t| t.is_available(manifest))
            .filter(|t| {
                filter
                    .map(|f| f.allows_tool(t.name(), t.tier()))
                    .unwrap_or(true)
            })
            .map(|t| with_intent_schema(t.definition()))
            .collect()
    }

    pub fn execute(
        &self,
        name: &str,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let tool = self
            .tools
            .iter()
            .find(|t| t.name() == name)
            .ok_or_else(|| anyhow::anyhow!("Unknown native tool '{}'", name))?;

        if !tool.is_available(manifest) {
            return Ok(ToolError::permission(
                format!("Native tool '{}' is not available or permitted", name),
            ).to_error_response());
        }

        tool.execute(
            manifest,
            policy,
            agent_dir,
            gateway_dir,
            arguments_json,
            session_id,
            turn_id,
            config,
            gateway_store,
            run_context,
        )
    }

    pub fn extract_metadata(&self, name: &str, arguments_json: &str) -> ToolMetadata {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.extract_metadata(arguments_json))
            .unwrap_or_default()
    }
}

pub(crate) fn tool_requires_intent(tool_name: &str) -> bool {
    tool_name == "sandbox_exec"
        || tool_name == "agent_spawn"
        || tool_name.starts_with("credential_")
        || tool_name.starts_with("agent_revision_")
        || tool_name.starts_with("scheduler_")
        || tool_name == "skill_normalize"
}

fn with_intent_schema(mut definition: ToolDefinition) -> ToolDefinition {
    let Some(schema) = definition.input_schema.as_object_mut() else {
        return definition;
    };

    let properties = schema.entry("properties").or_insert_with(|| json!({}));
    let Some(properties_map) = properties.as_object_mut() else {
        return definition;
    };

    properties_map.insert(
        "intent".to_string(),
        json!({
            "type": "string",
            "description": "Why you are invoking this tool in 1-2 sentences. Required for privileged tools and strongly encouraged everywhere else.",
            "maxLength": 500
        }),
    );

    if tool_requires_intent(&definition.name) {
        let required = schema
            .entry("required")
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Some(required_arr) = required.as_array_mut() {
            if !required_arr
                .iter()
                .any(|value| value.as_str() == Some("intent"))
            {
                required_arr.push(Value::String("intent".to_string()));
            }
        }
    }

    definition
}

pub(crate) fn validate_relative_agent_path(path: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!path.trim().is_empty(), "path must not be empty");
    anyhow::ensure!(
        !path.starts_with('/')
            && !path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == ".."),
        "path must stay within the agent directory"
    );
    Ok(())
}

pub(crate) fn validate_agent_id(agent_id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!agent_id.trim().is_empty(), "agent_id must not be empty");
    anyhow::ensure!(
        !agent_id.starts_with('.') && !agent_id.ends_with('.') && !agent_id.contains(".."),
        "agent_id must not start or end with '.', or contain '..'"
    );
    anyhow::ensure!(
        agent_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.'),
        "agent_id may only contain ASCII letters, digits, '.', '-' and '_'"
    );
    Ok(())
}

pub(crate) fn load_session_content_mounts(
    gateway_dir: Option<&Path>,
    session_id: &str,
) -> anyhow::Result<Vec<SandboxMount>> {
    let Some(gw_dir) = gateway_dir else {
        return Ok(Vec::new());
    };

    let store = match crate::runtime::content_store::ContentStore::new(gw_dir) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };

    let mut mounts = Vec::new();
    let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    let collect_mounts = |sid: &str,
                          mounts: &mut Vec<SandboxMount>,
                          seen: &mut std::collections::HashSet<String>|
     -> anyhow::Result<()> {
        let names_with_handles = match store.list_names_with_handles(sid) {
            Ok(n) => n,
            Err(_) => return Ok(()),
        };

        for (name, handle) in names_with_handles {
            if !seen.insert(name.clone()) {
                continue;
            }

            let content = match store.read(&handle) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let temp_base = std::env::temp_dir()
                .join("autonoetic_content")
                .join(session_id.replace('/', "_"));

            if let Err(_) = std::fs::create_dir_all(&temp_base) {
                continue;
            }

            let temp_file = temp_base.join(&name);
            if let Some(parent) = temp_file.parent() {
                if let Err(_) = std::fs::create_dir_all(parent) {
                    continue;
                }
            }

            if let Err(_) = std::fs::write(&temp_file, &content) {
                continue;
            }

            let dest_path = format!("/tmp/{}", name);

            mounts.push(SandboxMount {
                source: temp_file,
                dest: dest_path,
                readonly: false,
            });

            tracing::debug!(
                target: "sandbox",
                name = %name,
                handle = %handle,
                session = %sid,
                "Mounted session content file into sandbox"
            );
        }
        Ok(())
    };

    collect_mounts(session_id, &mut mounts, &mut seen_names)?;

    let manifest = store.load_manifest(session_id)?;
    if let Some(root_id) = &manifest.root_session_id {
        if root_id != session_id {
            collect_mounts(root_id, &mut mounts, &mut seen_names)?;
        }
    }

    Ok(mounts)
}

pub(crate) fn default_true() -> bool {
    true
}

pub(crate) fn build_approval_details(
    request: &ApprovalRequest,
    kind: &str,
    summary: String,
    retry_field: &str,
    subject: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "kind": kind,
        "reason": request.reason.clone().unwrap_or_else(|| "Approval required".to_string()),
        "summary": summary,
        "requested_by_agent_id": request.agent_id,
        "session_id": request.session_id,
        "retry_field": retry_field,
        "subject": subject
    })
}

pub(crate) fn extract_host(url: &str) -> anyhow::Result<String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| {
        anyhow::Error::from(tagged::Tagged::validation(anyhow::anyhow!(
            "Invalid URL '{}': {}",
            url,
            e
        )))
    })?;
    let host = parsed.host_str().ok_or_else(|| {
        anyhow::Error::from(tagged::Tagged::validation(anyhow::anyhow!(
            "URL '{}' does not contain a host",
            url
        )))
    })?;
    Ok(host.to_string())
}

pub(crate) fn block_on_http<F, T>(future: F) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| handle.block_on(future))
    } else {
        tokio::runtime::Runtime::new()?.block_on(future)
    }
}

pub(crate) fn tier2_memory_for_native_tool(
    gateway_dir: &Path,
    gateway_store: Option<&std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
    agent_id: &str,
    reader_session_id: Option<&str>,
) -> anyhow::Result<crate::runtime::memory::Tier2Memory> {
    let memory_store: Option<std::sync::Arc<dyn crate::runtime::memory::MemoryStore>> =
        gateway_store.map(|gs| {
            let store: std::sync::Arc<dyn crate::runtime::memory::MemoryStore> =
                std::sync::Arc::new(crate::runtime::memory::SqliteMemoryStore::new(gs.clone()));
            store
        });
    crate::runtime::memory::Tier2Memory::open_for_agent(
        gateway_dir,
        memory_store,
        agent_id,
        reader_session_id,
    )
}

/// Block on an async memory operation from a synchronous context (e.g. `NativeTool::execute`).
///
/// Uses `tokio::task::block_in_place` when inside a tokio runtime, otherwise
/// creates a new single-thread runtime. Same pattern as `block_on_http`.
pub(crate) fn block_on_memory<F, T>(fut: F) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| handle.block_on(fut))
    } else {
        tokio::runtime::Runtime::new()?.block_on(fut)
    }
}

pub fn resolve_target_to_agent_ref(
    target: &str,
    gateway_store: &crate::scheduler::gateway_store::GatewayStore,
) -> anyhow::Result<autonoetic_types::agent_revision::AgentRef> {
    let repo = crate::agent::repository::AgentRepository::new(std::path::PathBuf::new());
    let (agent_ref, _rev) = repo.resolve_agent(target, Some(gateway_store))?;
    Ok(agent_ref)
}

pub(crate) fn capability_type_name(cap: &Capability) -> String {
    match cap {
        Capability::SandboxFunctions { .. } => "SandboxFunctions".to_string(),
        Capability::ReadAccess { .. } => "ReadAccess".to_string(),
        Capability::WriteAccess { .. } => "WriteAccess".to_string(),
        Capability::NetworkAccess { .. } => "NetworkAccess".to_string(),
        Capability::AgentSpawn { .. } => "AgentSpawn".to_string(),
        Capability::AgentMessage { .. } => "AgentMessage".to_string(),
        Capability::BackgroundReevaluation { .. } => "BackgroundReevaluation".to_string(),
        Capability::CodeExecution { .. } => "CodeExecution".to_string(),
        Capability::EmergencyStop => "EmergencyStop".to_string(),
        Capability::AgentRevision { .. } => "AgentRevision".to_string(),
        Capability::Evaluation { .. } => "Evaluation".to_string(),
        Capability::ApprovalQueue { .. } => "ApprovalQueue".to_string(),
        Capability::SchedulerSignal { .. } => "SchedulerSignal".to_string(),
        Capability::CredentialAccess { .. } => "CredentialAccess".to_string(),
        Capability::UserProfileAccess { .. } => "UserProfileAccess".to_string(),
        Capability::SchedulerAccess { .. } => "SchedulerAccess".to_string(),
        Capability::SkillInstall { .. } => "SkillInstall".to_string(),
        Capability::ConstitutionalProposal { .. } => "ConstitutionalProposal".to_string(),
        Capability::ReasoningAudit { .. } => "ReasoningAudit".to_string(),
        Capability::BudgetNoPriceAvailableAllow => {
            "budget.no_price_available.allow".to_string()
        }
        Capability::GithubIssueCreate { .. } => "GithubIssueCreate".to_string(),
        Capability::SecurityRedTeam => "SecurityRedTeam".to_string(),
        Capability::CapsuleExport => "CapsuleExport".to_string(),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct SandboxExecDependencies {
    #[serde(default)]
    pub runtime: String,
    #[serde(default)]
    pub packages: Vec<String>,
}

fn sanitize_llm_token_artifacts(s: &str) -> String {
    let cleaned = s.replace("<|\"|>", "\"");
    cleaned.replace("<|'|>", "'")
}

fn fixup_unquoted_json_keys(s: &str) -> String {
    let re = regex::Regex::new(r"\b([a-zA-Z_][a-zA-Z0-9_]*)\s*:").unwrap();
    re.replace_all(s, "\"$1\":").to_string()
}

fn balance_braces(s: &str) -> String {
    let mut open_curly = 0i64;
    let mut open_bracket = 0i64;
    for ch in s.chars() {
        match ch {
            '{' => open_curly += 1,
            '}' => open_curly -= 1,
            '[' => open_bracket += 1,
            ']' => open_bracket -= 1,
            _ => {}
        }
    }
    let mut result = s.to_string();
    for _ in 0..open_bracket {
        result.push(']');
    }
    for _ in 0..open_curly {
        result.push('}');
    }
    result
}

fn parse_lenient_json_object<T: serde::de::DeserializeOwned>(
    s: &str,
) -> Result<T, serde_json::Error> {
    let sanitized = sanitize_llm_token_artifacts(s);
    match serde_json::from_str::<T>(&sanitized) {
        Ok(v) => Ok(v),
        Err(_) => {
            let fixed = fixup_unquoted_json_keys(&sanitized);
            match serde_json::from_str::<T>(&fixed) {
                Ok(v) => Ok(v),
                Err(_) => {
                    let balanced = balance_braces(&fixed);
                    serde_json::from_str::<T>(&balanced)
                }
            }
        }
    }
}

fn deserialize_deps_lenient<'de, D>(
    deserializer: D,
) -> Result<Option<SandboxExecDependencies>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let value = serde_json::Value::deserialize(deserializer)?;

    if value.is_null() {
        return Ok(None);
    }

    match serde_json::from_value::<SandboxExecDependencies>(value.clone()) {
        Ok(deps) => Ok(Some(deps)),
        Err(_) => {
            if let Some(s) = value.as_str() {
                parse_lenient_json_object::<SandboxExecDependencies>(s)
                    .map(Some)
                    .map_err(|e| {
                        Error::custom(format!(
                            "dependencies: expected object {{\"runtime\":\"...\", \"packages\":[...]}}, \
                             got string that could not be parsed: {e}"
                        ))
                    })
            } else {
                Err(Error::custom(format!(
                    "dependencies: expected object {{\"runtime\":\"...\", \"packages\":[...]}}, \
                     got {}",
                    value
                )))
            }
        }
    }
}

/// Accept a string or any JSON value; if given an object/array/number/bool,
/// serialize it back to a JSON string so downstream code always sees `String`.
///
/// Useful for fields like `agent.spawn.message` or `knowledge.store.content`
/// where weak function-callers pass structured data as raw objects.
pub fn deserialize_string_lenient<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Null => Err(Error::custom("expected string, got null")),
        other => Ok(other.to_string()),
    }
}

pub fn deserialize_string_map_values_lenient<'de, D>(
    deserializer: D,
) -> Result<std::collections::HashMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Null => Ok(std::collections::HashMap::new()),
        serde_json::Value::Object(map) => {
            let mut out = std::collections::HashMap::with_capacity(map.len());
            for (key, value) in map {
                let value = match value {
                    serde_json::Value::String(s) => s,
                    serde_json::Value::Null => {
                        return Err(Error::custom(format!(
                            "expected string, got null for key '{}'",
                            key
                        )));
                    }
                    other => other.to_string(),
                };
                out.insert(key, value);
            }
            Ok(out)
        }
        other => Err(Error::custom(format!("expected object, got {}", other))),
    }
}

/// Accept a boolean, integer, or common string representations of truthiness.
pub fn deserialize_bool_lenient<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Bool(b) => Ok(b),
        serde_json::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" | "" => Ok(false),
            _ => Err(Error::custom(format!("invalid boolean string: {}", s))),
        },
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(|v| v != 0)
            .ok_or_else(|| Error::custom("expected integer 0 or 1")),
        serde_json::Value::Null => Ok(false),
        _ => Err(Error::custom("expected boolean, string, or number")),
    }
}

/// Accept an integer or a string that parses as an integer.
pub fn deserialize_usize_lenient<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(n) => n
            .as_u64()
            .map(|v| v as usize)
            .ok_or_else(|| Error::custom("expected positive integer")),
        serde_json::Value::String(s) => s
            .trim()
            .parse::<usize>()
            .map_err(|e| Error::custom(format!("expected integer string: {e}"))),
        _ => Err(Error::custom("expected integer or string integer")),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct CapturePath {
    pub path: String,
    pub mount_as: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CredentialEnvMapping {
    pub credential_id: String,
    pub env_var: String,
}

/// Ensure `credential_id` looks like a credential reference and not a raw secret.
///
/// This keeps gateway behavior mechanical: only stable references should cross
/// tool boundaries, never secret material.
pub(crate) fn ensure_safe_credential_id_reference(credential_id: &str) -> anyhow::Result<()> {
    let id = credential_id.trim();
    anyhow::ensure!(!id.is_empty(), "credential_id must not be empty");
    anyhow::ensure!(
        id.len() <= 128,
        "credential_id is too long; expected a short credential reference"
    );
    anyhow::ensure!(
        id.starts_with("cred_"),
        "credential_id must use canonical reference format and start with 'cred_'"
    );
    anyhow::ensure!(
        id.len() > "cred_".len(),
        "credential_id must include an identifier after 'cred_'"
    );
    anyhow::ensure!(
        id.chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.'),
        "credential_id may only contain ASCII letters, digits, '_', '-' and '.'"
    );

    let looks_like_secret = looks_like_secret_value(id);

    anyhow::ensure!(
        !looks_like_secret,
        "credential_id must reference a stored credential (from credential.check), not a raw secret value"
    );

    Ok(())
}

pub(crate) fn implicit_artifact_id_error(tool_name: &str, artifact_id: &str) -> serde_json::Value {
    serde_json::json!({
        "ok": false,
        "error_type": "validation",
        "error": "invalid_artifact_id",
        "message": format!(
            "'{}' is an implicit task artifact (a content record), not an executable artifact bundle. {} only accepts artifact_ref handles produced by artifact_build.",
            artifact_id,
            tool_name
        ),
        "repair_hint": format!(
            "Call content.read('{}') to inspect the implicit artifact JSON. Pick an entry from content.artifacts[*].artifact_ref and retry {} with that artifact_ref.",
            artifact_id,
            tool_name
        ),
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct SandboxExecArgs {
    pub command: String,
    /// Free-text rationale for operators (recommended when execution may trigger approval).
    #[serde(default)]
    pub intent: Option<String>,
    #[serde(default, deserialize_with = "deserialize_deps_lenient")]
    pub dependencies: Option<SandboxExecDependencies>,
    #[serde(default)]
    pub approval_ref: Option<String>,
    #[serde(default)]
    pub artifact_id: Option<String>,
    /// Resolved server-side to the canonical `art_*` bundle id (same lookup as `artifact_exec`).
    #[serde(default)]
    pub artifact_ref: Option<String>,
    #[serde(default)]
    pub capture_paths: Option<Vec<CapturePath>>,
    #[serde(default)]
    pub credential_env: Option<Vec<CredentialEnvMapping>>,
}

fn parse_dependency_plan(runtime: &str, packages: Vec<String>) -> anyhow::Result<DependencyPlan> {
    let runtime = match runtime.to_ascii_lowercase().as_str() {
        "python" => DependencyRuntime::Python,
        "nodejs" | "node" => DependencyRuntime::NodeJs,
        other => return Err(tagged::Tagged::validation(anyhow::anyhow!("Unsupported dependency runtime '{}'", other)).into()),
    };
    Ok(DependencyPlan { runtime, packages })
}

pub(crate) fn dependency_plan_from_args_or_lock(
    manifest: &AgentManifest,
    agent_dir: &Path,
    deps: Option<SandboxExecDependencies>,
) -> anyhow::Result<Option<DependencyPlan>> {
    if let Some(deps) = deps {
        let runtime = if deps.runtime.is_empty() {
            "python"
        } else {
            deps.runtime.as_str()
        };
        return parse_dependency_plan(runtime, deps.packages).map(Some);
    }

    let lock_path = agent_dir.join(&manifest.runtime.runtime_lock);
    if !lock_path.exists() {
        return Ok(None);
    }
    let lock = crate::runtime_lock::resolve_runtime_lock(&lock_path)?;
    dependency_plan_from_lock(&lock)
}

pub(crate) fn dependency_plan_from_lock(
    lock: &autonoetic_types::runtime_lock::RuntimeLock,
) -> anyhow::Result<Option<DependencyPlan>> {
    if lock.dependencies.is_empty() {
        return Ok(None);
    }

    // Merge all dependency sets into a single plan.
    // All sets must target the same runtime; mixed runtimes require explicit
    // inline dependencies (the packager builds layers instead).
    let mut merged_packages: Vec<String> = Vec::new();
    let mut merged_runtime: Option<String> = None;
    for dep_set in &lock.dependencies {
        if let Some(ref rt) = merged_runtime {
            anyhow::ensure!(
                rt == &dep_set.runtime,
                "runtime.lock contains dependency sets with different runtimes ('{}' vs '{}'); use explicit dependencies or pre-built layers",
                rt, dep_set.runtime
            );
        } else {
            merged_runtime = Some(dep_set.runtime.clone());
        }
        merged_packages.extend(dep_set.packages.clone());
    }

    let runtime = merged_runtime.as_deref().unwrap_or("python");
    parse_dependency_plan(runtime, merged_packages).map(Some)
}

pub mod admin_proposal;

pub mod agent;
pub mod agent_revision;
pub mod approval;
pub mod artifact;
pub mod artifact_exec;
pub mod artifact_prepare;
pub mod agent_inspect;
pub mod capsule;
pub mod constitution;
pub mod content;
pub mod credential;
pub mod digest;
pub mod evaluation;
pub mod execution;
pub mod federation;
pub mod github_issue;
pub mod improvement;
pub mod knowledge;
pub mod observability;
pub mod promotion;
pub mod quality_trend;
pub mod sandbox;
pub mod scheduler;
pub mod security_redteam;
pub mod self_describe;
pub mod sentinel;
pub mod session;
pub mod skill;
pub mod user_interaction;
pub mod user_profile;
pub mod web;
pub mod workflow;

pub use crate::runtime::tools::agent_revision::{
    normalize_capability_from_llm, AgentRevisionCreateFromIntentTool, AgentRevisionCreateTool,
    AgentRevisionDiffTool, AgentRevisionInspectTool, AgentRevisionListTool,
    AgentRevisionPromoteTool, AgentRevisionRollbackTool,
};
pub use crate::runtime::tools::evaluation::{
    validate_suite_spec, EvalCompareTool, EvalReportTool, EvalRunTool, EvalSuiteCaseSpec,
    EvalSuitePublishTool, EvalSuiteSpec, EvalSuiteUpdateTool,
};
pub use crate::runtime::tools::security_redteam::{
    AttackPatternListTool, AttackPatternProposeTool,
};

pub fn default_registry() -> NativeToolRegistry {
    let mut registry = NativeToolRegistry::new();
    crate::runtime::tools::execution::register_tools(&mut registry);
    crate::runtime::tools::quality_trend::register_tools(&mut registry);
    crate::runtime::tools::digest::register_tools(&mut registry);
    crate::runtime::tools::session::register_tools(&mut registry);
    crate::runtime::tools::content::register_tools(&mut registry);
    crate::runtime::tools::agent::register_tools(&mut registry);
    crate::runtime::tools::agent_inspect::register_tools(&mut registry);
    crate::runtime::tools::agent_revision::register_tools(&mut registry);
    crate::runtime::tools::approval::register_tools(&mut registry);
    crate::runtime::tools::evaluation::register_tools(&mut registry);
    crate::runtime::tools::credential::register_tools(&mut registry);
    crate::runtime::tools::web::register_tools(&mut registry);
    crate::runtime::tools::artifact::register_tools(&mut registry);
    crate::runtime::tools::artifact_exec::register_tools(&mut registry);
    crate::runtime::tools::artifact_prepare::register_tools(&mut registry);
    crate::runtime::tools::knowledge::register_tools(&mut registry);
    crate::runtime::tools::agent::register_tools(&mut registry);
    crate::runtime::tools::sandbox::register_tools(&mut registry);
    crate::runtime::tools::workflow::register_tools(&mut registry);
    crate::runtime::tools::user_interaction::register_tools(&mut registry);
    crate::runtime::tools::user_profile::register_tools(&mut registry);
    crate::runtime::tools::observability::register_tools(&mut registry);
    crate::runtime::tools::federation::register_tools(&mut registry);
    crate::runtime::tools::improvement::register_tools(&mut registry);
    crate::runtime::tools::github_issue::register_tools(&mut registry);
    crate::runtime::tools::promotion::register_tools(&mut registry);
    crate::runtime::tools::scheduler::register_tools(&mut registry);
    crate::runtime::tools::skill::register_tools(&mut registry);
    crate::runtime::tools::admin_proposal::register_tools(&mut registry);
    crate::runtime::tools::capsule::register_tools(&mut registry);
    crate::runtime::tools::self_describe::register_tools(&mut registry);
    crate::runtime::tools::constitution::register_tools(&mut registry);
    crate::runtime::tools::security_redteam::register_tools(&mut registry);
    crate::runtime::tools::sentinel::register_tools(&mut registry);
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_tier_filter_allows_all_when_empty() {
        let filter = ToolTierFilter::all();
        assert!(filter.allows("content_write"));
        assert!(filter.allows("web_search"));
        assert!(filter.allows("agent_spawn"));
    }

    #[test]
    fn test_tool_tier_filter_core_only() {
        let filter = ToolTierFilter::core_only();
        assert!(filter.allows("content_write"));
        assert!(filter.allows("sandbox_exec"));
        assert!(!filter.allows("web_search"));
        assert!(!filter.allows("agent_spawn"));
    }

    #[test]
    fn test_tool_tier_filter_core_and_workflow() {
        let filter = ToolTierFilter::core_and_workflow();
        assert!(filter.allows("content_write"));
        assert!(filter.allows("agent_spawn"));
        assert!(!filter.allows("web_search"));
        assert!(!filter.allows("promotion_record"));
    }

    #[test]
    fn credential_id_reference_requires_canonical_format() {
        assert!(ensure_safe_credential_id_reference("cred_openweather_api").is_ok());
        assert!(ensure_safe_credential_id_reference("openweather_api").is_err());
        assert!(ensure_safe_credential_id_reference("cred_").is_err());
        assert!(ensure_safe_credential_id_reference("cred_bad/value").is_err());
    }

    #[test]
    fn test_tool_tier_filter_approval_exception() {
        let filter = ToolTierFilter {
            allowed_tiers: vec![ToolTier::Core],
            always_include_approval_tools: true,
            always_include_inspection_tools: false,
            clarification_read_only: false,
            allow_promotion_record_without_specialized_tier: false,
        };
        assert!(!filter.allows("web_search"));
        assert!(filter.allows("approval_list"));
        assert!(filter.allows("approval_answer"));
    }

    #[test]
    fn test_tool_tier_filter_core_and_workflow_with_approvals() {
        let filter = ToolTierFilter::core_and_workflow_with_approvals();
        assert!(filter.allows("content_write"));
        assert!(filter.allows("sandbox_exec"));
        assert!(filter.allows("agent_spawn"));
        assert!(filter.allows("approval_status"));
        assert!(filter.allows("workflow_state"));
        assert!(!filter.allows("web_search"));
        assert!(!filter.allows("promotion_record"));
        assert!(!filter.allows("agent_revision_create"));
    }

    #[test]
    fn test_available_definitions_filtered_no_filter_equals_all() {
        let registry = default_registry();
        let manifest = AgentManifest {
            version: "1.0".to_string(),
            runtime: autonoetic_types::agent::RuntimeDeclaration {
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: autonoetic_types::agent::AgentIdentity {
                id: "test-agent".to_string(),
                name: "test".to_string(),
                description: "test".to_string(),
            },
            capabilities: vec![],
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            execution_mode: Default::default(),
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            agentskills_import: None,
            compression: None,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        };
        let unfiltered = registry.available_definitions(&manifest);
        let filtered = registry.available_definitions_filtered(&manifest, None);
        assert_eq!(unfiltered.len(), filtered.len());
    }

    #[test]
    fn test_available_definitions_filtered_core_only() {
        let registry = default_registry();
        let manifest = AgentManifest {
            version: "1.0".to_string(),
            runtime: autonoetic_types::agent::RuntimeDeclaration {
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: autonoetic_types::agent::AgentIdentity {
                id: "test-agent".to_string(),
                name: "test".to_string(),
                description: "test".to_string(),
            },
            capabilities: vec![],
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            execution_mode: Default::default(),
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            agentskills_import: None,
            compression: None,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        };
        let filter = ToolTierFilter::core_only();
        let filtered = registry.available_definitions_filtered(&manifest, Some(&filter));
        for def in &filtered {
            assert_eq!(
                tool_tier(&def.name),
                ToolTier::Core,
                "Core-only filter should exclude {}",
                def.name
            );
        }
    }

    #[test]
    fn test_sandbox_exec_args_dependencies_struct() {
        let args: SandboxExecArgs = serde_json::from_str(
            r#"{"command": "python3 /tmp/test.py", "dependencies": {"runtime": "python", "packages": ["requests", "pandas"]}}"#,
        ).unwrap();
        assert_eq!(args.command, "python3 /tmp/test.py");
        let deps = args.dependencies.unwrap();
        assert_eq!(deps.runtime, "python");
        assert_eq!(deps.packages, vec!["requests", "pandas"]);
    }

    #[test]
    fn test_sandbox_exec_args_dependencies_none() {
        let args: SandboxExecArgs =
            serde_json::from_str(r#"{"command": "python3 /tmp/test.py"}"#).unwrap();
        assert!(args.dependencies.is_none());
        assert!(args.intent.is_none());
    }

    #[test]
    fn test_sandbox_exec_args_optional_intent() {
        let args: SandboxExecArgs = serde_json::from_str(
            r#"{"command": "python3 /tmp/main.py", "intent": "Run unit tests"}"#,
        )
        .unwrap();
        assert_eq!(args.intent.as_deref(), Some("Run unit tests"));
    }

    #[test]
    fn test_sandbox_exec_args_optional_artifact_ref() {
        let args: SandboxExecArgs = serde_json::from_str(
            r#"{"command": "python3 /tmp/main.py", "artifact_ref": "ar.demo"}"#,
        )
        .unwrap();
        assert_eq!(args.artifact_ref.as_deref(), Some("ar.demo"));
        assert!(args.artifact_id.is_none());
    }

    #[test]
    fn test_sandbox_exec_args_dependencies_stringified_json() {
        let args: SandboxExecArgs = serde_json::from_str(
            r#"{"command": "python3 /tmp/test.py", "dependencies": "{\"runtime\": \"python\", \"packages\": [\"requests\"]}"}"#,
        ).unwrap();
        let deps = args.dependencies.unwrap();
        assert_eq!(deps.runtime, "python");
        assert_eq!(deps.packages, vec!["requests"]);
    }

    #[test]
    fn test_sandbox_exec_args_dependencies_string_with_marker_tokens() {
        let args: SandboxExecArgs = serde_json::from_str(
            r#"{"command": "python3 /tmp/test.py", "dependencies": "{packages:[<|\"|>requests<|\"|>]"}"#,
        ).unwrap();
        let deps = args.dependencies.unwrap();
        assert_eq!(deps.packages, vec!["requests"]);
    }

    #[test]
    fn test_sandbox_exec_args_dependencies_stringified_runtime_only_with_markers() {
        let args: SandboxExecArgs = serde_json::from_str(
            r#"{"command": "python3 /tmp/test.py", "dependencies": "{runtime:<|\"|>python<|\"|>}"}"#,
        ).unwrap();
        let deps = args.dependencies.unwrap();
        assert_eq!(deps.runtime, "python");
        assert!(deps.packages.is_empty());
    }

    #[test]
    fn test_sandbox_exec_args_dependencies_null() {
        let args: SandboxExecArgs =
            serde_json::from_str(r#"{"command": "python3 /tmp/test.py", "dependencies": null}"#)
                .unwrap();
        assert!(args.dependencies.is_none());
    }

    #[test]
    fn test_sandbox_exec_args_dependencies_garbage_string_fails() {
        let result = serde_json::from_str::<SandboxExecArgs>(
            r#"{"command": "python3 /tmp/test.py", "dependencies": "not json at all"}"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_dependency_plan_from_args_or_lock_explicit_deps_override() {
        let manifest = AgentManifest {
            version: "1.0".to_string(),
            runtime: autonoetic_types::agent::RuntimeDeclaration {
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: autonoetic_types::agent::AgentIdentity {
                id: "test-agent".to_string(),
                name: "test".to_string(),
                description: "test".to_string(),
            },
            capabilities: vec![],
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            execution_mode: Default::default(),
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            agentskills_import: None,
            compression: None,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        };
        let temp_dir = tempfile::tempdir().unwrap();
        let deps = Some(SandboxExecDependencies {
            runtime: "python".to_string(),
            packages: vec!["requests".to_string()],
        });
        let plan = dependency_plan_from_args_or_lock(&manifest, temp_dir.path(), deps).unwrap();
        assert!(plan.is_some());
        let plan = plan.unwrap();
        assert_eq!(plan.runtime, DependencyRuntime::Python);
        assert_eq!(plan.packages, vec!["requests"]);
    }

    #[test]
    fn test_dependency_plan_from_args_or_lock_no_lock_file_returns_none() {
        let manifest = AgentManifest {
            version: "1.0".to_string(),
            runtime: autonoetic_types::agent::RuntimeDeclaration {
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: autonoetic_types::agent::AgentIdentity {
                id: "test-agent".to_string(),
                name: "test".to_string(),
                description: "test".to_string(),
            },
            capabilities: vec![],
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            execution_mode: Default::default(),
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            agentskills_import: None,
            compression: None,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        };
        let temp_dir = tempfile::tempdir().unwrap();
        let plan = dependency_plan_from_args_or_lock(&manifest, temp_dir.path(), None).unwrap();
        assert!(plan.is_none());
    }

    #[test]
    fn test_deserialize_string_lenient_from_string() {
        #[derive(Deserialize)]
        struct Args {
            #[serde(deserialize_with = "deserialize_string_lenient")]
            message: String,
        }
        let args: Args = serde_json::from_str(r#"{"message": "hello"}"#).unwrap();
        assert_eq!(args.message, "hello");
    }

    #[test]
    fn test_deserialize_string_lenient_from_object() {
        #[derive(Deserialize)]
        struct Args {
            #[serde(deserialize_with = "deserialize_string_lenient")]
            message: String,
        }
        let args: Args = serde_json::from_str(r#"{"message": {"location": "Paris"}}"#).unwrap();
        assert_eq!(args.message, r#"{"location":"Paris"}"#);
    }

    #[test]
    fn test_deserialize_string_lenient_from_array() {
        #[derive(Deserialize)]
        struct Args {
            #[serde(deserialize_with = "deserialize_string_lenient")]
            message: String,
        }
        let args: Args = serde_json::from_str(r#"{"message": [1, 2, 3]}"#).unwrap();
        assert_eq!(args.message, "[1,2,3]");
    }

    #[test]
    fn test_deserialize_string_lenient_from_number() {
        #[derive(Deserialize)]
        struct Args {
            #[serde(deserialize_with = "deserialize_string_lenient")]
            message: String,
        }
        let args: Args = serde_json::from_str(r#"{"message": 42}"#).unwrap();
        assert_eq!(args.message, "42");
    }

    #[test]
    fn test_deserialize_string_map_values_lenient_stringifies_nested_values() {
        #[derive(Deserialize)]
        struct Args {
            #[serde(deserialize_with = "deserialize_string_map_values_lenient")]
            env: std::collections::HashMap<String, String>,
        }

        let args: Args = serde_json::from_str(
            r#"{"env":{"AUTONOETIC_INPUT":{"location":"Paris","date":"tomorrow"},"FLAG":true,"COUNT":3}}"#,
        )
        .unwrap();

        let nested = args
            .env
            .get("AUTONOETIC_INPUT")
            .expect("env var should exist");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(nested).unwrap(),
            serde_json::json!({"location": "Paris", "date": "tomorrow"})
        );
        assert_eq!(args.env.get("FLAG").map(String::as_str), Some("true"));
        assert_eq!(args.env.get("COUNT").map(String::as_str), Some("3"));
    }

    #[test]
    fn test_deserialize_string_map_values_lenient_preserves_plain_strings() {
        #[derive(Deserialize)]
        struct Args {
            #[serde(deserialize_with = "deserialize_string_map_values_lenient")]
            env: std::collections::HashMap<String, String>,
        }

        let args: Args = serde_json::from_str(
            r#"{"env":{"AUTONOETIC_INPUT":"{\"location\":\"Paris\",\"date\":\"tomorrow\"}"}}"#,
        )
        .unwrap();

        assert_eq!(
            args.env.get("AUTONOETIC_INPUT").map(String::as_str),
            Some(r#"{"location":"Paris","date":"tomorrow"}"#)
        );
    }

    #[test]
    fn test_deserialize_bool_lenient_from_bool() {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default, deserialize_with = "deserialize_bool_lenient")]
            flag: bool,
        }
        let args: Args = serde_json::from_str(r#"{"flag": true}"#).unwrap();
        assert!(args.flag);
    }

    #[test]
    fn test_deserialize_bool_lenient_from_string_true() {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default, deserialize_with = "deserialize_bool_lenient")]
            flag: bool,
        }
        let args: Args = serde_json::from_str(r#"{"flag": "true"}"#).unwrap();
        assert!(args.flag);
    }

    #[test]
    fn test_deserialize_bool_lenient_from_string_one() {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default, deserialize_with = "deserialize_bool_lenient")]
            flag: bool,
        }
        let args: Args = serde_json::from_str(r#"{"flag": "1"}"#).unwrap();
        assert!(args.flag);
    }

    #[test]
    fn test_deserialize_bool_lenient_from_number() {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default, deserialize_with = "deserialize_bool_lenient")]
            flag: bool,
        }
        let args: Args = serde_json::from_str(r#"{"flag": 1}"#).unwrap();
        assert!(args.flag);
    }

    #[test]
    fn test_deserialize_bool_lenient_from_null_defaults_false() {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default, deserialize_with = "deserialize_bool_lenient")]
            flag: bool,
        }
        let args: Args = serde_json::from_str(r#"{}"#).unwrap();
        assert!(!args.flag);
    }

    #[test]
    fn test_deserialize_usize_lenient_from_number() {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default, deserialize_with = "deserialize_usize_lenient")]
            limit: usize,
        }
        let args: Args = serde_json::from_str(r#"{"limit": 3000}"#).unwrap();
        assert_eq!(args.limit, 3000);
    }

    #[test]
    fn test_deserialize_usize_lenient_from_string() {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default, deserialize_with = "deserialize_usize_lenient")]
            limit: usize,
        }
        let args: Args = serde_json::from_str(r#"{"limit": "3000"}"#).unwrap();
        assert_eq!(args.limit, 3000);
    }
}
