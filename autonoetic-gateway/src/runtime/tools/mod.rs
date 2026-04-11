use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::prompt_budget::tool_tier;
use crate::sandbox::{DependencyPlan, DependencyRuntime, SandboxMount};
use autonoetic_types::agent::{AgentManifest, ToolTier};
use autonoetic_types::background::ApprovalRequest;
use autonoetic_types::capability::Capability;
use autonoetic_types::tool_error::tagged;
use serde::Deserialize;

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
}

impl ToolTierFilter {
    /// Create a filter that allows only Core tier tools.
    pub fn core_only() -> Self {
        Self {
            allowed_tiers: vec![ToolTier::Core],
            always_include_approval_tools: false,
        }
    }

    /// Create a filter that allows Core and Workflow tier tools.
    pub fn core_and_workflow() -> Self {
        Self {
            allowed_tiers: vec![ToolTier::Core, ToolTier::Workflow],
            always_include_approval_tools: false,
        }
    }

    /// Create a filter that allows all tiers (no filtering).
    pub fn all() -> Self {
        Self {
            allowed_tiers: vec![],
            always_include_approval_tools: false,
        }
    }

    /// Check if a tool with the given name passes this filter.
    /// Derives tier from the tool name prefix.
    /// Also respects always_include_approval_tools for approval-prefixed tools.
    pub fn allows(&self, tool_name: &str) -> bool {
        if self.allowed_tiers.is_empty() {
            return true;
        }
        if self.always_include_approval_tools && tool_name.starts_with("approval.") {
            return true;
        }
        self.allows_tier(tool_tier(tool_name))
    }

    /// Check if a tool with the given name and tier passes this filter.
    /// Use this when the tier is already known (e.g. from NativeTool::tier()).
    /// Also respects always_include_approval_tools for approval-prefixed tools.
    pub fn allows_tool(&self, tool_name: &str, tier: ToolTier) -> bool {
        if self.allowed_tiers.is_empty() {
            return true;
        }
        if self.always_include_approval_tools && tool_name.starts_with("approval.") {
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
            .map(|t| t.definition())
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
            .map(|t| t.definition())
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
            anyhow::bail!("Native tool '{}' is not available or permitted", name);
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
                std::sync::Arc::new(
                    crate::runtime::memory::SqliteMemoryStore::new(gs.clone()),
                );
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

pub(crate) fn resolve_target_to_agent_ref(
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

#[derive(Debug, Deserialize)]
pub(crate) struct CapturePath {
    pub path: String,
    pub mount_as: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SandboxExecArgs {
    pub command: String,
    #[serde(default, deserialize_with = "deserialize_deps_lenient")]
    pub dependencies: Option<SandboxExecDependencies>,
    #[serde(default)]
    pub approval_ref: Option<String>,
    #[serde(default)]
    pub artifact_id: Option<String>,
    #[serde(default)]
    pub capture_paths: Option<Vec<CapturePath>>,
}

fn parse_dependency_plan(runtime: &str, packages: Vec<String>) -> anyhow::Result<DependencyPlan> {
    let runtime = match runtime.to_ascii_lowercase().as_str() {
        "python" => DependencyRuntime::Python,
        "nodejs" | "node" => DependencyRuntime::NodeJs,
        other => anyhow::bail!("Unsupported dependency runtime '{}'", other),
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
    if lock.dependencies.is_empty() {
        return Ok(None);
    }
    anyhow::ensure!(
        lock.dependencies.len() == 1,
        "runtime.lock currently supports exactly one dependency set"
    );
    let locked = &lock.dependencies[0];
    parse_dependency_plan(locked.runtime.as_str(), locked.packages.clone()).map(Some)
}

pub mod agent;
pub mod agent_revision;
pub mod artifact;
pub mod content;
pub mod credential;
pub mod digest;
pub mod evaluation;
pub mod execution;
pub mod knowledge;
pub mod promotion;
pub mod sandbox;
pub mod session;
pub mod user_interaction;
pub mod user_profile;
pub mod web;
pub mod workflow;

pub use crate::runtime::tools::agent_revision::{
    AgentRevisionCreateFromIntentTool, AgentRevisionCreateTool, AgentRevisionDiffTool,
    AgentRevisionInspectTool, AgentRevisionListTool, AgentRevisionPromoteTool,
    AgentRevisionRollbackTool,
};
pub use crate::runtime::tools::evaluation::{
    validate_suite_spec, EvalCompareTool, EvalReportTool, EvalRunTool, EvalSuiteCaseSpec,
    EvalSuitePublishTool, EvalSuiteSpec,
};

pub fn default_registry() -> NativeToolRegistry {
    let mut registry = NativeToolRegistry::new();
    crate::runtime::tools::execution::register_tools(&mut registry);
    crate::runtime::tools::digest::register_tools(&mut registry);
    crate::runtime::tools::session::register_tools(&mut registry);
    crate::runtime::tools::content::register_tools(&mut registry);
    crate::runtime::tools::agent_revision::register_tools(&mut registry);
    crate::runtime::tools::evaluation::register_tools(&mut registry);
    crate::runtime::tools::credential::register_tools(&mut registry);
    crate::runtime::tools::web::register_tools(&mut registry);
    crate::runtime::tools::artifact::register_tools(&mut registry);
    crate::runtime::tools::knowledge::register_tools(&mut registry);
    crate::runtime::tools::agent::register_tools(&mut registry);
    crate::runtime::tools::sandbox::register_tools(&mut registry);
    crate::runtime::tools::workflow::register_tools(&mut registry);
    crate::runtime::tools::user_interaction::register_tools(&mut registry);
    crate::runtime::tools::user_profile::register_tools(&mut registry);
    crate::runtime::tools::promotion::register_tools(&mut registry);
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_tier_filter_allows_all_when_empty() {
        let filter = ToolTierFilter::all();
        assert!(filter.allows("content.write"));
        assert!(filter.allows("web.search"));
        assert!(filter.allows("agent.spawn"));
    }

    #[test]
    fn test_tool_tier_filter_core_only() {
        let filter = ToolTierFilter::core_only();
        assert!(filter.allows("content.write"));
        assert!(filter.allows("sandbox.exec"));
        assert!(!filter.allows("web.search"));
        assert!(!filter.allows("agent.spawn"));
    }

    #[test]
    fn test_tool_tier_filter_core_and_workflow() {
        let filter = ToolTierFilter::core_and_workflow();
        assert!(filter.allows("content.write"));
        assert!(filter.allows("agent.spawn"));
        assert!(!filter.allows("web.search"));
        assert!(!filter.allows("promotion.record"));
    }

    #[test]
    fn test_tool_tier_filter_approval_exception() {
        let filter = ToolTierFilter {
            allowed_tiers: vec![ToolTier::Core],
            always_include_approval_tools: true,
        };
        assert!(!filter.allows("web.search"));
        assert!(filter.allows("approval.status"));
        assert!(filter.allows("approval.answer"));
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
            gateway_url: None,
            gateway_token: None,
            response_contract: None,
            allowed_tool_tiers: vec![],
            agentskills_import: None,
        compression: None,
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
            gateway_url: None,
            gateway_token: None,
            response_contract: None,
            allowed_tool_tiers: vec![],
            agentskills_import: None,
        compression: None,
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
}
