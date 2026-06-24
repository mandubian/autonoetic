use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::human_gate::{GateKind, GateRequest, GateResult, GateService};
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use crate::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::wiki::{WikiGetResult, WikiListResult, WikiPageEntry};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(WikiListTool));
    registry.register(Box::new(WikiGetTool));
    registry.register(Box::new(WikiProposeTool));
}

pub fn resolve_wiki_dir(gateway_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(gd) = gateway_dir {
        let dir = gd.join("wiki");
        if dir.exists() {
            return Some(dir);
        }
    }
    let fallback = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("docs")
        .join("wiki");
    if fallback.exists() {
        Some(fallback)
    } else {
        None
    }
}

#[derive(Debug, Clone, Deserialize)]
struct IndexEntry {
    id: String,
    title: String,
    file: String,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct WikiIndex {
    pages: Vec<IndexEntry>,
}

fn load_index(wiki_dir: &Path) -> anyhow::Result<WikiIndex> {
    let index_path = wiki_dir.join("index.toml");
    let content = std::fs::read_to_string(&index_path).map_err(|e| {
        anyhow::anyhow!("failed to read wiki index '{}': {}", index_path.display(), e)
    })?;
    toml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse wiki index.toml: {}", e))
}

pub fn list_pages(wiki_dir: Option<&Path>) -> anyhow::Result<WikiListResult> {
    let dir = resolve_wiki_dir(wiki_dir).ok_or_else(|| {
        anyhow::anyhow!("wiki directory not found (neither .gateway/wiki/ nor source tree)")
    })?;
    let index = load_index(&dir)?;
    let pages: Vec<WikiPageEntry> = index
        .pages
        .into_iter()
        .map(|p| WikiPageEntry {
            id: p.id,
            title: p.title,
            tags: p.tags,
        })
        .collect();
    Ok(WikiListResult { pages })
}

pub fn get_page(wiki_dir: Option<&Path>, id: &str) -> anyhow::Result<WikiGetResult> {
    let dir = resolve_wiki_dir(wiki_dir).ok_or_else(|| {
        anyhow::anyhow!("wiki directory not found")
    })?;
    let index = load_index(&dir)?;

    let entry = index.pages.iter().find(|p| p.id == id).ok_or_else(|| {
        let available: Vec<&str> = index.pages.iter().map(|p| p.id.as_str()).collect();
        anyhow::anyhow!(
            "wiki page '{}' not found. Available: {}",
            id,
            available.join(", ")
        )
    })?;

    let file_path = dir.join(&entry.file);
    let content = std::fs::read_to_string(&file_path).map_err(|e| {
        anyhow::anyhow!("failed to read wiki page '{}': {}", file_path.display(), e)
    })?;

    Ok(WikiGetResult {
        id: entry.id.clone(),
        title: entry.title.clone(),
        content,
        tags: entry.tags.clone(),
    })
}

pub struct WikiListTool;

impl NativeTool for WikiListTool {
    fn name(&self) -> &'static str {
        "wiki_list"
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "List all available wiki pages in the knowledge corpus. \
                Returns page IDs, titles, and tags. Use wiki.get to retrieve the full \
                content of a specific page. Start here to discover what documentation \
                is available about the Autonoetic platform, tools, SDK, and architecture."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        _arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<Arc<GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let result = list_pages(gateway_dir)?;
        Ok(serde_json::to_string(&result)?)
    }
}

pub struct WikiGetTool;

impl NativeTool for WikiGetTool {
    fn name(&self) -> &'static str {
        "wiki_get"
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Get the full content of a wiki page by its ID. \
                Use wiki.list first to discover available page IDs. \
                Returns the page title, content (markdown), and tags."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "The page ID from wiki.list (e.g. 'sdk-python', 'architecture-overview', 'approval-system')"
                    }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<Arc<GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            id: String,
        }
        let args: Args = serde_json::from_str(arguments_json).map_err(|e| {
            anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e)
        })?;
        let result = get_page(gateway_dir, &args.id)?;
        Ok(serde_json::to_string(&result)?)
    }
}

pub struct WikiProposeTool;

impl NativeTool for WikiProposeTool {
    fn name(&self) -> &'static str {
        "wiki_propose"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::WikiContribute))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Propose a new wiki page or edit an existing one. \
                Requires WikiContribute capability. The proposal creates a gate \
                that the operator must approve before the page is published. \
                Returns immediately with the gate reference — the agent is NOT \
                suspended. Use approval.status to check if the proposal is still \
                pending, approved, or rejected."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "pattern": "^[a-z0-9]+(-[a-z0-9]+)*$",
                        "description": "Page ID (lowercase, hyphens allowed, e.g. 'runbook-agent-creation')"
                    },
                    "title": {
                        "type": "string",
                        "description": "Human-readable page title"
                    },
                    "content": {
                        "type": "string",
                        "description": "Full markdown content (max 64 KiB)"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional tags for categorization"
                    }
                },
                "required": ["id", "title", "content"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<Arc<GatewayStore>>,
        run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct ProposeArgs {
            id: String,
            title: String,
            content: String,
            #[serde(default)]
            tags: Vec<String>,
        }
        let args: ProposeArgs = serde_json::from_str(arguments_json).map_err(|e| {
            anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e)
        })?;

        // Validate id pattern
        let re = regex::Regex::new(r"^[a-z0-9]+(-[a-z0-9]+)*$").unwrap();
        if !re.is_match(&args.id) {
            anyhow::bail!(
                "Invalid page id '{}': must match pattern [a-z0-9]+(-[a-z0-9]+)*",
                args.id
            );
        }

        // Validate content size
        if args.content.is_empty() {
            anyhow::bail!("Content must not be empty");
        }
        if args.content.len() > 65536 {
            anyhow::bail!("Content exceeds maximum size of 64 KiB");
        }

        // Check if this is an edit (page already exists)
        let is_edit = resolve_wiki_dir(gateway_dir)
            .map(|dir| dir.join(format!("{}.md", &args.id)).exists())
            .unwrap_or(false);

        // Get session reference
        let sid = session_id.unwrap_or("unknown").to_string();
        let agent_id = manifest.agent.id.clone();

        // Build GateKind
        let kind = GateKind::WikiProposal {
            page_id: args.id.clone(),
            title: args.title.clone(),
            content: args.content.clone(),
            tags: args.tags.clone(),
            is_edit,
            proposed_by_agent: agent_id,
            proposed_by_session: sid,
        };

        // Run through GateService
        let store = gateway_store
            .ok_or_else(|| anyhow::anyhow!("GatewayStore not available"))?;
        let gate = GateService::new(store);
        let gate_req = GateRequest {
            kind,
            manifest,
            session_id,
            run_context,
            config,
            context: crate::runtime::human_gate::DecisionContext::tier2(
                format!(
                    "wiki {} \"{}\" ({})",
                    if is_edit { "edit" } else { "new" },
                    args.title,
                    args.id
                ),
                "agent proposes a wiki change for review",
                "publishes agent-authored content to the wiki",
                "Approve if the proposed wiki content is accurate and appropriate to publish; reject if it is inaccurate, low-quality, or out of scope",
            ),
            summary: format!("Wiki proposal: {}", args.title),
            approval_ref: None,
            pre_validated: false,
            cache_backfill: None,
            turn_id: None,
        };

        let result = gate.check(gate_req)?;

        match result {
            GateResult::Cleared { .. } => {
                // Should not happen for WikiProposal; return success with no gate_id
                Ok(serde_json::json!({
                    "ok": true,
                    "id": args.id,
                    "is_edit": is_edit,
                    "status": "approved"
                })
                .to_string())
            }
            GateResult::AlreadyPending { gate_id, .. } => {
                Ok(serde_json::json!({
                    "ok": true,
                    "id": args.id,
                    "gate_id": gate_id,
                    "is_edit": is_edit,
                    "status": "pending",
                    "proposed_at": chrono::Utc::now().to_rfc3339(),
                })
                .to_string())
            }
            GateResult::Suspended { gate_id, response_json, .. } => {
                // Return the gate_id — the agent is NOT suspended for wiki proposals
                let mut resp: serde_json::Value =
                    serde_json::from_str(&response_json).unwrap_or_default();
                if let Some(obj) = resp.as_object_mut() {
                    obj.insert("id".to_string(), serde_json::Value::String(args.id));
                    obj.insert("is_edit".to_string(), serde_json::Value::Bool(is_edit));
                }
                Ok(resp.to_string())
            }
            GateResult::PolicyAllowed => {
                Ok(serde_json::json!({
                    "ok": true,
                    "id": args.id,
                    "is_edit": is_edit,
                    "status": "approved"
                })
                .to_string())
            }
        }
    }
}
