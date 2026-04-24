use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{block_on_memory, tier2_memory_for_native_tool, NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use serde::Deserialize;
use std::path::Path;

/// Maps tool-facing retention labels to optional `expires_at` (RFC 3339 UTC).
fn knowledge_retention_expires_at(retention: &str) -> anyhow::Result<Option<String>> {
    let r = retention.trim().to_ascii_lowercase();
    let now = chrono::Utc::now();
    match r.as_str() {
        "stable" | "" => Ok(None),
        // Short-lived facts (e.g. spot prices); avoids cluttering long-term knowledge.
        "ephemeral" => Ok(Some((now + chrono::Duration::hours(1)).to_rfc3339())),
        "1d" => Ok(Some((now + chrono::Duration::days(1)).to_rfc3339())),
        "30d" => Ok(Some((now + chrono::Duration::days(30)).to_rfc3339())),
        other => anyhow::bail!(
            "retention must be one of: stable, ephemeral, 1d, 30d (got {:?})",
            other
        ),
    }
}

fn parse_knowledge_store_visibility(
    raw: &str,
    tool_session_id: Option<&str>,
) -> anyhow::Result<autonoetic_types::memory::MemoryVisibility> {
    use autonoetic_types::memory::MemoryVisibility;
    match raw.trim().to_ascii_lowercase().as_str() {
        "private" => Ok(MemoryVisibility::Private),
        "global" => Ok(MemoryVisibility::Global),
        "session" => {
            let sid = tool_session_id
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "knowledge.store visibility \"session\" requires a non-empty tool session_id"
                    )
                })?;
            Ok(MemoryVisibility::Session {
                session_id: sid.to_string(),
            })
        }
        other => anyhow::bail!(
            "visibility must be private, session, or global (got {:?})",
            other
        ),
    }
}

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(KnowledgeStoreTool));
    registry.register(Box::new(KnowledgeRecallTool));
    registry.register(Box::new(KnowledgeSearchTool));
    registry.register(Box::new(KnowledgeSearchByTagsTool));
    registry.register(Box::new(DigestQueryTool));
}

pub struct KnowledgeStoreTool;

impl NativeTool for KnowledgeStoreTool {
    fn name(&self) -> &'static str {
        "knowledge.store"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::WriteAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Store a durable fact in the knowledge base with provenance. Default visibility is session: any agent in the same session can read it; use private to restrict to yourself, or global for all agents. Use retention for TTL: stable (default), ephemeral (~1 hour), 1d, or 30d. To widen visibility later, call knowledge.store again with the same id. IMPORTANT: 'content' must be a plain string — never a JSON object. If you want to store structured data, serialize it to a JSON string first (e.g., JSON.stringify or serde_json::to_string).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Unique identifier for this knowledge" },
                    "content": { "type": "string", "description": "The fact or information to store. Must be a plain string — not a JSON object. If storing structured data, serialize it to a JSON string first." },
                    "scope": { "type": "string", "description": "Category/namespace for organizing knowledge (e.g., 'api-keys', 'user-preferences')", "default": "general" },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Tags for searchability" },
                    "confidence": { "type": "number", "description": "Confidence level (0.0 to 1.0)", "default": 1.0 },
                    "retention": { "type": "string", "description": "Lifetime: stable (no expiry), ephemeral (~1 hour), 1d, 30d", "default": "stable" },
                    "visibility": { "type": "string", "description": "Who can read: session (default, same session), private (writer/owner only), global (all agents)", "default": "session" }
                },
                "required": ["id", "content"],
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
        turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            id: String,
            #[serde(deserialize_with = "crate::runtime::tools::deserialize_string_lenient")]
            content: String,
            #[serde(default = "default_scope")]
            scope: String,
            #[serde(default)]
            tags: Vec<String>,
            #[serde(default = "default_confidence")]
            confidence: f64,
            #[serde(default = "default_retention")]
            retention: String,
            #[serde(default = "default_visibility")]
            visibility: String,
        }
        fn default_scope() -> String {
            "general".to_string()
        }
        fn default_confidence() -> f64 {
            1.0
        }
        fn default_retention() -> String {
            "stable".to_string()
        }
        fn default_visibility() -> String {
            "session".to_string()
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.id.trim().is_empty(), "id must not be empty");
        anyhow::ensure!(!args.content.trim().is_empty(), "content must not be empty");
        anyhow::ensure!(
            args.confidence >= 0.0 && args.confidence <= 1.0,
            "confidence must be between 0.0 and 1.0"
        );

        let Some(gw_dir) = gateway_dir else {
            anyhow::bail!("Knowledge requires gateway directory to be configured");
        };

        let sid = session_id.unwrap_or(&manifest.agent.id);
        let source_ref = match turn_id {
            Some(tid) => format!("session:{}:turn:{}", sid, tid),
            None => format!("session:{}", sid),
        };

        let mem = tier2_memory_for_native_tool(
            gw_dir,
            gateway_store.as_ref(),
            &manifest.agent.id,
            session_id,
        )?;

        let expires_at = knowledge_retention_expires_at(&args.retention)?;
        let visibility = parse_knowledge_store_visibility(&args.visibility, session_id)?;

        let mut memory = autonoetic_types::memory::MemoryObject::new(
            args.id.clone(),
            args.scope.clone(),
            manifest.agent.id.clone(),
            manifest.agent.id.clone(),
            source_ref,
            args.content.clone(),
        );
        memory.confidence = Some(args.confidence);
        memory.tags = args.tags.clone();
        memory.expires_at = expires_at.clone();
        memory.visibility = visibility;

        if let Some(sid) = session_id {
            let store = gateway_store.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "knowledge.store requires gateway_store to tag revision/binding provenance for session-bound writes"
                )
            })?;
            if let Ok(Some(binding)) = store.get_session_agent_binding(sid) {
                memory.revision_id = Some(binding.revision_id.clone());
                memory.binding_session_id = Some(binding.session_id.clone());
                memory.alias_ref = binding.alias_id.clone();
            }
        }

        let memory = block_on_memory(mem.save_memory(&memory))?;

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "id": memory.memory_id,
            "scope": memory.scope,
            "content_hash": memory.content_hash,
            "created_at": memory.created_at,
            "expires_at": memory.expires_at,
            "retention": args.retention,
            "visibility": memory.visibility,
        }))
        .map_err(Into::into)
    }
}

pub struct KnowledgeRecallTool;

impl NativeTool for KnowledgeRecallTool {
    fn name(&self) -> &'static str {
        "knowledge.recall"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::ReadAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Recall a durable fact from the knowledge base by its ID. Respects visibility and access control - you can only recall knowledge you have access to.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The knowledge ID to recall" }
                },
                "required": ["id"],
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
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            id: String,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.id.trim().is_empty(), "id must not be empty");

        let Some(gw_dir) = gateway_dir else {
            anyhow::bail!("Knowledge requires gateway directory to be configured");
        };

        let mem = tier2_memory_for_native_tool(
            gw_dir,
            gateway_store.as_ref(),
            &manifest.agent.id,
            session_id,
        )?;
        let memory = block_on_memory(mem.recall(&args.id))?;

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "id": memory.memory_id,
            "content": memory.content,
            "scope": memory.scope,
            "writer": memory.writer_agent_id,
            "created_at": memory.created_at,
            "confidence": memory.confidence,
            "expires_at": memory.expires_at,
        }))
        .map_err(Into::into)
    }
}

pub struct KnowledgeSearchTool;

impl NativeTool for KnowledgeSearchTool {
    fn name(&self) -> &'static str {
        "knowledge.search"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::ReadAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Search the knowledge base by scope and optional query. Returns all knowledge in the scope that you have access to, optionally filtered by content matching the query.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "scope": { "type": "string", "description": "The scope/namespace to search in (e.g., 'api-keys', 'user-preferences')" },
                    "query": { "type": "string", "description": "Optional search term to filter by content" }
                },
                "required": ["scope"],
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
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            scope: String,
            query: Option<String>,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.scope.trim().is_empty(), "scope must not be empty");

        let Some(gw_dir) = gateway_dir else {
            anyhow::bail!("Knowledge requires gateway directory to be configured");
        };

        let mem = tier2_memory_for_native_tool(
            gw_dir,
            gateway_store.as_ref(),
            &manifest.agent.id,
            session_id,
        )?;
        let results = block_on_memory(mem.search(&args.scope, args.query.as_deref()))?;

        let items: Vec<serde_json::Value> = results
            .iter()
            .map(|m| {
                serde_json::json!({
                    "id": m.memory_id,
                    "content": m.content,
                    "writer": m.writer_agent_id,
                    "created_at": m.created_at,
                    "confidence": m.confidence,
                    "expires_at": m.expires_at,
                })
            })
            .collect();

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "scope": args.scope,
            "results": items,
            "count": items.len(),
        }))
        .map_err(Into::into)
    }
}

pub struct KnowledgeSearchByTagsTool;

impl NativeTool for KnowledgeSearchByTagsTool {
    fn name(&self) -> &'static str {
        "knowledge.search_by_tags"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::ReadAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Search the knowledge base by scope and tags. Each result's `tags` JSON array must contain every tag you pass (AND semantics). Optional `text` filters `content` with a SQL LIKE substring match.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "scope": { "type": "string", "description": "Scope/namespace (e.g. 'lessons', 'general')" },
                    "tags": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "All of these tag strings must appear in the record's tags list" },
                    "text": { "type": "string", "description": "Optional substring filter on content" },
                    "limit": { "type": "integer", "description": "Max results (1–100)", "default": 10 }
                },
                "required": ["scope", "tags"],
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
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            scope: String,
            tags: Vec<String>,
            text: Option<String>,
            #[serde(default = "default_limit")]
            limit: u32,
        }
        fn default_limit() -> u32 {
            10
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.scope.trim().is_empty(), "scope must not be empty");
        anyhow::ensure!(!args.tags.is_empty(), "tags must be a non-empty array");
        anyhow::ensure!(
            (1..=100).contains(&args.limit),
            "limit must be between 1 and 100 inclusive"
        );
        let limit = args.limit as usize;

        let Some(gw_dir) = gateway_dir else {
            anyhow::bail!("Knowledge requires gateway directory to be configured");
        };

        let mem = tier2_memory_for_native_tool(
            gw_dir,
            gateway_store.as_ref(),
            &manifest.agent.id,
            session_id,
        )?;
        let results = block_on_memory(mem.search_by_tags(
            &args.scope,
            &args.tags,
            args.text.as_deref(),
            limit,
        ))?;

        let items: Vec<serde_json::Value> = results
            .iter()
            .map(|m| {
                serde_json::json!({
                    "id": m.memory_id,
                    "content": m.content,
                    "scope": m.scope,
                    "tags": m.tags,
                    "writer": m.writer_agent_id,
                    "created_at": m.created_at,
                    "confidence": m.confidence,
                    "expires_at": m.expires_at,
                })
            })
            .collect();

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "scope": args.scope,
            "tags": args.tags,
            "results": items,
            "count": items.len(),
        }))
        .map_err(Into::into)
    }
}

fn truncate_narrative_to_char_boundary(s: &str, max_chars: usize) -> String {
    let max_chars = max_chars.max(1);
    let mut count = 0usize;
    let mut end_byte = 0usize;
    for (i, c) in s.char_indices() {
        if count >= max_chars {
            break;
        }
        count += 1;
        end_byte = i + c.len_utf8();
    }
    if end_byte >= s.len() {
        s.to_string()
    } else {
        format!("{}… (truncated)", &s[..end_byte])
    }
}

pub struct DigestQueryTool;

impl NativeTool for DigestQueryTool {
    fn name(&self) -> &'static str {
        "digest.query"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::ReadAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Search digest-scoped Tier-2 memories by scope and tags, and optionally load the post-session narrative: either as `post_session_narrative.md` for the session root, or by explicit content handle/alias via `narrative_handle` (uses the same resolution rules as `content.read`).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "scope": { "type": "string", "description": "Memory scope/namespace (e.g. 'digest.lesson')" },
                    "tags": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "AND-matched tags on the memory record" },
                    "text": { "type": "string", "description": "Optional substring filter on memory content" },
                    "session_id": { "type": "string", "description": "Session id for resolving narrative by name or handle (see `narrative_handle`). If omitted, the active tool session id is used when available." },
                    "narrative_handle": { "type": "string", "description": "Optional content handle (sha256:…), short alias, or name for the post-session narrative blob. Requires `session_id` or an active tool session for visibility checks." },
                    "narrative_max_chars": { "type": "integer", "description": "Max Unicode scalars of narrative to return (default 16000)", "default": 16000 },
                    "limit": { "type": "integer", "description": "Max memory results (1–100, default 10)", "default": 10 }
                },
                "required": ["scope", "tags"],
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
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            scope: String,
            tags: Vec<String>,
            text: Option<String>,
            #[serde(default)]
            session_id: Option<String>,
            #[serde(default)]
            narrative_handle: Option<String>,
            #[serde(default = "default_narrative_cap")]
            narrative_max_chars: usize,
            #[serde(default = "default_limit")]
            limit: u32,
        }
        fn default_limit() -> u32 {
            10
        }
        fn default_narrative_cap() -> usize {
            16_000
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.scope.trim().is_empty(), "scope must not be empty");
        anyhow::ensure!(!args.tags.is_empty(), "tags must be non-empty");
        anyhow::ensure!((1..=100).contains(&args.limit), "limit must be 1–100");

        let Some(gw_dir) = gateway_dir else {
            anyhow::bail!("digest.query requires gateway directory");
        };

        let reader_sid = args.session_id.as_deref().or(session_id);
        let mem = tier2_memory_for_native_tool(
            gw_dir,
            gateway_store.as_ref(),
            &manifest.agent.id,
            reader_sid,
        )?;
        let results = block_on_memory(mem.search_by_tags(
            &args.scope,
            &args.tags,
            args.text.as_deref(),
            args.limit as usize,
        ))?;

        let items: Vec<serde_json::Value> = results
            .iter()
            .map(|m| {
                serde_json::json!({
                    "id": m.memory_id,
                    "content": m.content,
                    "scope": m.scope,
                    "tags": m.tags,
                    "writer": m.writer_agent_id,
                    "created_at": m.created_at,
                    "confidence": m.confidence,
                    "expires_at": m.expires_at,
                })
            })
            .collect();

        let sid_for_narrative = args
            .session_id
            .as_deref()
            .or(session_id)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let narrative = if let Some(ref raw) = args.narrative_handle {
            let nh = raw.trim();
            anyhow::ensure!(
                !nh.is_empty(),
                "narrative_handle must be non-empty when provided"
            );
            let sid = sid_for_narrative.ok_or_else(|| {
                anyhow::anyhow!(
                    "digest.query narrative_handle requires session_id (argument) or an active tool session context"
                )
            })?;
            let store = crate::runtime::content_store::ContentStore::new(gw_dir)?;
            let bytes = store.read_by_name_or_handle(sid, nh)?;
            let text = String::from_utf8(bytes)
                .map_err(|e| anyhow::anyhow!("narrative content is not valid UTF-8: {e}"))?;
            let truncated =
                truncate_narrative_to_char_boundary(&text, args.narrative_max_chars.max(1));
            Some(serde_json::json!({
                "session_id": sid,
                "handle_or_name": nh,
                "text": truncated,
            }))
        } else if let Some(sid_raw) = sid_for_narrative {
            let base = crate::runtime::live_digest::base_session_id(sid_raw).to_string();
            let store = crate::runtime::content_store::ContentStore::new(gw_dir)?;
            match store.read_by_name(
                &base,
                crate::runtime::post_session_digest::POST_SESSION_NARRATIVE_CONTENT_NAME,
            ) {
                Ok(bytes) => {
                    let text = String::from_utf8(bytes).map_err(|e| {
                        anyhow::anyhow!("post_session_narrative.md is not valid UTF-8: {e}")
                    })?;
                    let truncated =
                        truncate_narrative_to_char_boundary(&text, args.narrative_max_chars.max(1));
                    Some(serde_json::json!({
                        "root_session_id": base,
                        "name": crate::runtime::post_session_digest::POST_SESSION_NARRATIVE_CONTENT_NAME,
                        "text": truncated,
                    }))
                }
                Err(_) => None,
            }
        } else {
            None
        };

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "scope": args.scope,
            "tags": args.tags,
            "memories": items,
            "memory_count": items.len(),
            "narrative": narrative,
        }))
        .map_err(Into::into)
    }
}
