//! Agent-initiated capsule tools: `capsule.export` / `capsule.import`.
//!
//! Both gated by [`Capability::CapsuleExport`].

use crate::capsule::{export, import, ExportContext, ExportRequest, ImportContext, ImportRequest};
use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::NativeTool;
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::capsule::CapsuleMode;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub fn register_tools(registry: &mut crate::runtime::tools::NativeToolRegistry) {
    registry.register(Box::new(CapsuleExportTool));
    registry.register(Box::new(CapsuleImportTool));
}

fn parse_mode(s: &str) -> anyhow::Result<CapsuleMode> {
    match s.to_ascii_lowercase().as_str() {
        "thin" => Ok(CapsuleMode::Thin),
        "hermetic" => Ok(CapsuleMode::Hermetic),
        "replay" => Ok(CapsuleMode::Replay),
        "headless" => Ok(CapsuleMode::Headless),
        other => anyhow::bail!(
            "unknown capsule mode '{}' (expected: thin | hermetic | replay | headless)",
            other
        ),
    }
}

// --- Export ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CapsuleExportArgs {
    agent_id: String,
    #[serde(default = "default_mode_string")]
    mode: String,
    #[serde(default)]
    revision: Option<String>,
    #[serde(default)]
    include_memory: Option<bool>,
    #[serde(default)]
    sign: Option<bool>,
    #[serde(default)]
    output: Option<PathBuf>,
    /// Required when `mode == "replay"`.
    #[serde(default)]
    session_id: Option<String>,
    /// Required when `mode == "headless"`.
    #[serde(default)]
    root_session_id: Option<String>,
}

fn default_mode_string() -> String {
    "thin".to_string()
}

pub struct CapsuleExportTool;

impl NativeTool for CapsuleExportTool {
    fn name(&self) -> &'static str {
        "capsule.export"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::CapsuleExport))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Export an agent revision as a Cognitive Capsule archive. \
                          Requires the CapsuleExport capability."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string" },
                    "mode": { "type": "string", "enum": ["thin", "hermetic", "replay", "headless"] },
                    "revision": { "type": ["string", "null"] },
                    "include_memory": { "type": ["boolean", "null"] },
                    "sign": { "type": ["boolean", "null"] },
                    "output": { "type": ["string", "null"] }
                },
                "required": ["agent_id"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let decision = policy.can_use_capsule();
        if !decision.is_allowed() {
            return Err(anyhow::anyhow!(
                "CapsuleExport capability is required to invoke '{}'",
                self.name()
            ));
        }
        let args: CapsuleExportArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;
        let config = config.ok_or_else(|| {
            anyhow::anyhow!(
                "{} requires a gateway config in scope",
                self.name()
            )
        })?;
        let store = gateway_store.ok_or_else(|| {
            anyhow::anyhow!(
                "{} requires the gateway store in scope",
                self.name()
            )
        })?;
        let gateway_dir = gateway_dir.ok_or_else(|| {
            anyhow::anyhow!(
                "{} requires the gateway_dir in scope",
                self.name()
            )
        })?;

        let mode = parse_mode(&args.mode)?;
        let outcome = export(
            ExportRequest {
                agent_id: args.agent_id,
                revision_id: args.revision,
                mode,
                include_memory: args.include_memory,
                sign: args.sign,
                output_path: args.output,
                session_id: args.session_id,
                root_session_id: args.root_session_id,
            },
            ExportContext {
                gateway_dir,
                gateway_config: config,
                gateway_store: &store,
            },
        )?;
        Ok(serde_json::to_string(&outcome)?)
    }
}

// --- Import ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CapsuleImportArgs {
    archive: PathBuf,
    #[serde(default)]
    verify_signature: bool,
    #[serde(default)]
    activate: bool,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    trust_domain: Option<String>,
}

pub struct CapsuleImportTool;

impl NativeTool for CapsuleImportTool {
    fn name(&self) -> &'static str {
        "capsule.import"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::CapsuleExport))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Import a Cognitive Capsule archive on this gateway. \
                          Requires the CapsuleExport capability."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "archive": { "type": "string" },
                    "verify_signature": { "type": "boolean" },
                    "activate": { "type": "boolean" },
                    "dry_run": { "type": "boolean" },
                    "trust_domain": { "type": ["string", "null"] }
                },
                "required": ["archive"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let decision = policy.can_use_capsule();
        if !decision.is_allowed() {
            return Err(anyhow::anyhow!(
                "CapsuleExport capability is required to invoke '{}'",
                self.name()
            ));
        }
        let args: CapsuleImportArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;
        let config = config.ok_or_else(|| {
            anyhow::anyhow!("{} requires a gateway config in scope", self.name())
        })?;
        let store = gateway_store.ok_or_else(|| {
            anyhow::anyhow!("{} requires the gateway store in scope", self.name())
        })?;
        let gateway_dir = gateway_dir.ok_or_else(|| {
            anyhow::anyhow!("{} requires the gateway_dir in scope", self.name())
        })?;

        let outcome = import(
            ImportRequest {
                archive_path: args.archive,
                verify_signature: args.verify_signature,
                activate: args.activate,
                dry_run: args.dry_run,
                trust_domain_override: args.trust_domain,
                ..Default::default()
            },
            ImportContext {
                gateway_dir,
                gateway_config: config,
                gateway_store: &store,
            },
        )?;
        Ok(serde_json::to_string(&outcome)?)
    }
}
