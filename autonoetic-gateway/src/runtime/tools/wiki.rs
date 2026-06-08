use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use crate::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::wiki::{WikiGetResult, WikiListResult, WikiPageEntry};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(WikiListTool));
    registry.register(Box::new(WikiGetTool));
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
        "wiki.list"
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
        "wiki.get"
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
