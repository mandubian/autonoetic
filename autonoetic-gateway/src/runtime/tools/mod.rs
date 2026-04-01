use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::sandbox::{DependencyPlan, DependencyRuntime, SandboxMount};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::background::ApprovalRequest;
use autonoetic_types::capability::Capability;
use autonoetic_types::tool_error::tagged;
use serde::Deserialize;

use std::path::Path;

#[derive(Debug, Default)]
pub struct ToolMetadata {
    pub path: Option<String>,
}

pub trait NativeTool: Send + Sync {
    fn name(&self) -> &'static str;
    fn definition(&self) -> ToolDefinition;
    fn is_available(&self, manifest: &AgentManifest) -> bool;

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
) -> anyhow::Result<crate::runtime::memory::Tier2Memory> {
    crate::runtime::memory::Tier2Memory::open_for_agent(
        gateway_dir,
        gateway_store.cloned(),
        agent_id,
    )
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
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct SandboxExecDependencies {
    pub runtime: String,
    pub packages: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CapturePath {
    pub path: String,
    pub mount_as: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SandboxExecArgs {
    pub command: String,
    #[serde(default)]
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
        return parse_dependency_plan(deps.runtime.as_str(), deps.packages).map(Some);
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
pub mod digest;
pub mod evaluation;
pub mod execution;
pub mod knowledge;
pub mod sandbox;
pub mod session;
pub mod user_interaction;
pub mod web;
pub mod workflow;

pub use crate::runtime::tools::agent_revision::{
    AgentRevisionCreateTool, AgentRevisionDiffTool, AgentRevisionInspectTool,
    AgentRevisionListTool, AgentRevisionPromoteTool, AgentRevisionRollbackTool,
};
pub use crate::runtime::tools::evaluation::{
    validate_suite_spec, EvalCompareTool, EvalReportTool, EvalRunTool, EvalSuiteCaseSpec,
    EvalSuitePublishTool, EvalSuiteSpec,
};
pub use crate::runtime::tools_impl::default_registry;
