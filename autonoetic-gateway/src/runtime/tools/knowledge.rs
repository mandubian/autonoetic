use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{
    block_on_memory, tier2_memory_for_native_tool, NativeTool, NativeToolRegistry,
};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::tool_error::ToolError;
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
        other => {
            return Err(autonoetic_types::tool_error::tagged::Tagged::validation(anyhow::anyhow!(
                "retention must be one of: stable, ephemeral, 1d, 30d (got {:?})",
                other
            )).into());
        }
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
        other => {
            return Err(autonoetic_types::tool_error::tagged::Tagged::validation(anyhow::anyhow!(
                "visibility must be private, session, or global (got {:?})",
                other
            )).into());
        }
    }
}

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(KnowledgeStoreTool));
    registry.register(Box::new(KnowledgeRecallTool));
    registry.register(Box::new(KnowledgeSearchTool));
    registry.register(Box::new(DigestQueryTool));
}

/// Compute a deterministic memory ID from scope + normalized content.
/// Same content always produces the same ID → `memory_upsert` becomes
/// an idempotent update rather than a duplicate insert (#868).
pub(crate) fn deterministic_id(scope: &str, content: &str) -> String {
    let normalized: String = content
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    // `dedup-<sha256(scope|normalized)[..24]>` — hashing the concatenation
    // matches the previous streamed updates exactly.
    format!(
        "dedup-{}",
        autonoetic_types::id_format::hash_and_truncate(&format!("{scope}|{normalized}"), 24)
    )
}

pub struct KnowledgeStoreTool;

impl NativeTool for KnowledgeStoreTool {
    fn name(&self) -> &'static str {
        "knowledge_store"
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
            description: "Store a durable fact in the knowledge base with provenance. Default visibility is global: all agents across sessions can read it; use session to restrict to the same session, or private to restrict to yourself. Use retention for TTL: stable (default), ephemeral (~1 hour), 1d, or 30d. To widen visibility later, call knowledge.store again with the same id. IMPORTANT: 'content' must be a plain string — never a JSON object. If you want to store structured data, serialize it to a JSON string first (e.g., JSON.stringify or serde_json::to_string).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Unique identifier for this knowledge. Optional — omit to auto-generate a deterministic ID from the content (recommended for dedup: re-storing the same pattern becomes an update, not a duplicate insert)." },
                    "content": { "type": "string", "description": "The fact or information to store. Must be a plain string — not a JSON object. If storing structured data, serialize it to a JSON string first." },
                    "scope": { "type": "string", "description": "Category/namespace for organizing knowledge (e.g., 'api-keys', 'user-preferences')", "default": "general" },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Tags for searchability" },
                    "confidence": { "type": "number", "description": "Confidence level (0.0 to 1.0)", "default": 1.0 },
                    "retention": { "type": "string", "description": "Lifetime: stable (no expiry), ephemeral (~1 hour), 1d, 30d", "default": "stable" },
                    "visibility": { "type": "string", "description": "Who can read: global (default, all agents across sessions), session (same session only), private (writer/owner only)", "default": "global" }
                },
                "required": ["content"],
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
            #[serde(default)]
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
            "global".to_string()
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let user_provided_id = !args.id.trim().is_empty();
        let id = if user_provided_id {
            args.id.clone()
        } else {
            deterministic_id(&args.scope, &args.content)
        };

        anyhow::ensure!(!args.content.trim().is_empty(), "content must not be empty");
        anyhow::ensure!(
            args.confidence >= 0.0 && args.confidence <= 1.0,
            "confidence must be between 0.0 and 1.0"
        );

        let Some(gw_dir) = gateway_dir else {
            return Ok(ToolError::resource("Knowledge requires gateway directory to be configured", None::<String>).to_error_response());
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

        // ── Semantic dedup (Jaccard pre-check) ────────────────────────
        // When the ID is auto-computed, check for an existing pattern in
        // the same scope with the same `type:` tag whose Jaccard token
        // overlap exceeds the configured threshold. If found, reuse that
        // entry's ID so the upsert merges instead of inserting a duplicate.
        let id = if user_provided_id {
            id
        } else {
            match block_on_memory(mem.recall(&id)).ok() {
                Some(_) => id, // exact match already exists
                None => {
                    let type_tag = args.tags.iter().find(|t| t.starts_with("type:"));
                    match type_tag {
                        Some(tag) => {
                            let threshold = _config
                                .map(|c| c.knowledge_store.similarity_threshold)
                                .unwrap_or(0.25);
                            let candidates = block_on_memory(
                                mem.search_by_tags(&args.scope, &[tag.clone()], None, 20),
                            )
                            .unwrap_or_default();
                            let mut best_score = 0.0_f64;
                            let mut best_id = None;
                            for candidate in &candidates {
                                if candidate.memory_id == id {
                                    continue;
                                }
                                let score =
                                    crate::runtime::context::score_task_relevance(
                                        &candidate.content,
                                        &args.content,
                                    );
                                if score > best_score {
                                    best_score = score;
                                    best_id = Some(candidate.memory_id.clone());
                                }
                            }
                            if best_score >= threshold {
                                if let Some(ref merged) = best_id {
                                    tracing::info!(
                                        target: "knowledge_store",
                                        new_id = %id,
                                        merged_into = %merged,
                                        score = %best_score,
                                        threshold = %threshold,
                                        "semantic dedup: merging with existing pattern (Jaccard {:.2})",
                                        best_score
                                    );
                                    merged.clone()
                                } else {
                                    id
                                }
                            } else {
                                id
                            }
                        }
                        None => id, // no type tag → exact match only
                    }
                }
            }
        };

        let expires_at = knowledge_retention_expires_at(&args.retention)?;
        let visibility = parse_knowledge_store_visibility(&args.visibility, session_id)?;

        let mut memory = autonoetic_types::memory::MemoryObject::new(
            id,
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
        "knowledge_recall"
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
            return Ok(ToolError::resource("Knowledge requires gateway directory to be configured", None::<String>).to_error_response());
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
        "knowledge_search"
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
            description: "Search the knowledge base in a scope using full-text search (FTS5, with stemming). Without `tags`, returns scope contents optionally filtered by `query`. With `tags` (AND semantics — every tag must be present on a record), filters to tagged records, with `query` as an optional full-text search filter on content. Results are capped at `limit` (default 10, max 100). Tip: FTS5 uses a porter stemmer, so \"trade\" matches \"trading\" and \"traded\".".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "scope": { "type": "string", "description": "The scope/namespace to search in (e.g., 'api-keys', 'lessons')" },
                    "query": { "type": "string", "description": "Optional full-text search on content (FTS5 MATCH syntax — uses porter stemmer)" },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional AND-match tags: every tag listed must appear on a record's tags list" },
                    "limit": { "type": "integer", "description": "Max results (1–100)", "default": 10 }
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
            #[serde(default)]
            tags: Vec<String>,
            #[serde(default = "default_search_limit")]
            limit: u32,
        }
        fn default_search_limit() -> u32 {
            10
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.scope.trim().is_empty(), "scope must not be empty");
        anyhow::ensure!(
            (1..=100).contains(&args.limit),
            "limit must be between 1 and 100 inclusive"
        );
        let limit = args.limit as usize;

        let Some(gw_dir) = gateway_dir else {
            return Ok(ToolError::resource("Knowledge requires gateway directory to be configured", None::<String>).to_error_response());
        };

        let mem = tier2_memory_for_native_tool(
            gw_dir,
            gateway_store.as_ref(),
            &manifest.agent.id,
            session_id,
        )?;

        // Tags present → AND-match tag search; otherwise plain scope/content search.
        let results = if args.tags.is_empty() {
            let mut r = block_on_memory(mem.search(&args.scope, args.query.as_deref()))?;
            r.truncate(limit);
            r
        } else {
            block_on_memory(mem.search_by_tags(
                &args.scope,
                &args.tags,
                args.query.as_deref(),
                limit,
            ))?
        };

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
        "digest_query"
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
            return Ok(ToolError::resource("digest.query requires gateway directory", None::<String>).to_error_response());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_id_is_stable() {
        let a = deterministic_id("evolution/patterns", "When make is blocked by sandbox policy P-1.9");
        let b = deterministic_id("evolution/patterns", "When make is blocked by sandbox policy P-1.9");
        assert_eq!(a, b);
        assert!(a.starts_with("dedup-"));
    }

    #[test]
    fn deterministic_id_differs_on_content() {
        let a = deterministic_id("evolution/patterns", "When make is blocked by sandbox policy");
        let b = deterministic_id("evolution/patterns", "Python repr prevents truncation in file reads");
        assert_ne!(a, b);
    }

    #[test]
    fn deterministic_id_differs_on_scope() {
        let a = deterministic_id("evolution/patterns", "sandbox exec permission error");
        let b = deterministic_id("digest.lesson", "sandbox exec permission error");
        assert_ne!(a, b);
    }

    #[test]
    fn jaccard_high_for_same_concept_different_phrasing() {
        let a = "When `make` is blocked by sandbox policy P-1.9, wrap in bash -c or call python3 directly.";
        let b = "sandbox_exec permission blocks bare make commands. Workaround: use bash -c wrapper or python3 scripts directly.";
        let score = crate::runtime::context::score_task_relevance(a, b);
        assert!(score >= 0.25, "expected >= 0.25 for same-concept phrases, got {score:.3}");
    }

    #[test]
    fn jaccard_low_for_different_patterns() {
        let a = "When `make` is blocked by sandbox policy P-1.9, wrap in bash -c.";
        let b = "The session transcript handle is not visible to cross-agent callers.";
        let score = crate::runtime::context::score_task_relevance(a, b);
        assert!(score < 0.25, "expected < 0.25 for unrelated patterns, got {score:.3}");
    }

    #[test]
    fn jaccard_distinguishes_sandbox_subtopics() {
        let a = "make blocked by sandbox policy P-1.9 — use python3 directly.";
        let b = "sandbox filesystem isolation causes resolve content_not_found for host paths.";
        let score = crate::runtime::context::score_task_relevance(a, b);
        // shared: "sandbox" + "by" — not enough to exceed 0.25
        assert!(score < 0.25, "expected < 0.25 for different sandbox subtopics, got {score:.3}");
    }
}
