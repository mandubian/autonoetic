use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::egress_stored::{
    filter_or_indicate_for_sink, query_sink_or_remote, resolve_stored_label, FilteredStoredContent,
};
use crate::runtime::human_gate::{GateKind, GateRequest, GateResult, GateService};
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use crate::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::egress::{EgressLabel, IndicationVerbosity, NamedEgressLabel};
use autonoetic_types::wiki::{WikiGetResult, WikiListResult, WikiPageEntry};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Parse optional `egress_label:` from YAML frontmatter (RFC §6 wiki surface).
///
/// Returns `Ok(None)` when the key is absent (legacy default applies at filter
/// time). Returns `Err` when the key is present but the value is unrecognized —
/// fail-closed rather than silently widening to `legacy_unlabeled`.
fn wiki_frontmatter_egress_label(content: &str) -> anyhow::Result<Option<EgressLabel>> {
    let t = content.trim_start();
    if !t.starts_with("---") {
        return Ok(None);
    }
    let mut lines = t.lines();
    let _ = lines.next();
    for line in lines {
        if line == "---" {
            break;
        }
        let line = line.trim();
        let Some(rest) = line
            .strip_prefix("egress_label:")
            .or_else(|| line.strip_prefix("egress-label:"))
        else {
            continue;
        };
        let raw = rest
            .split('#')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('"')
            .trim_matches('\'');
        if raw.is_empty() {
            anyhow::bail!("wiki page frontmatter: egress_label is present but empty");
        }
        let named: NamedEgressLabel = serde_json::from_str(&format!("\"{raw}\""))
            .map_err(|_| {
                anyhow::anyhow!(
                    "wiki page frontmatter: unrecognized egress_label '{raw}' \
                     (expected unrestricted | local_only | no_remote_model)"
                )
            })?;
        return Ok(Some(named.to_label()));
    }
    Ok(None)
}

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
    /// Repo-relative path of the human doc this page digests.
    ///
    /// Seven wiki pages share a basename with a doc under `docs/` — the same
    /// subject written for two different readers. That is legitimate (a wiki
    /// page is a short digest served into an agent's context; the doc is the
    /// human reference) but nothing said which one was authoritative, so both
    /// drifted. Recording it here rather than in the page body keeps it out of
    /// the prompt: `index.toml` is parsed, never served.
    ///
    /// Validated by `every_page_names_a_canonical_doc_that_exists`.
    #[serde(default)]
    canonical: Option<String>,
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
                Returns page IDs, titles, and tags. Use wiki_get to retrieve the full \
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
                Use wiki_list first to discover available page IDs. \
                Returns the page title, content (markdown), and tags."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "The page ID from wiki_list (e.g. 'sdk-python', 'architecture-overview', 'approval-system')"
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
        config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<Arc<GatewayStore>>,
        run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            id: String,
        }
        let args: Args = serde_json::from_str(arguments_json).map_err(|e| {
            anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e)
        })?;
        let mut result = get_page(gateway_dir, &args.id)?;
        let cfg = config
            .map(|c| c.egress.clone())
            .unwrap_or_default();
        let sink = query_sink_or_remote(run_context.and_then(|rc| rc.egress_query_sink));
        let stored = wiki_frontmatter_egress_label(&result.content)?;
        let label = resolve_stored_label(stored.as_ref(), &cfg);
        match filter_or_indicate_for_sink(
            &result.content,
            &label,
            sink,
            Some("wiki_get"),
            IndicationVerbosity::Descriptive,
        ) {
            FilteredStoredContent::Allowed(_) => {}
            FilteredStoredContent::Withheld { indication } => {
                result.content = indication;
            }
        }
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
            request_id: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every page in `index.toml` must resolve to a readable file.
    ///
    /// The index and the files are maintained by hand, so a typo in `file =` or
    /// a page added to the directory but not the index fails silently: the
    /// agent-facing `wiki_get` just errors on a page `wiki_list` advertised.
    #[test]
    fn every_indexed_page_resolves() {
        let listed = list_pages(None).expect("built-in corpus index should parse");
        assert!(
            !listed.pages.is_empty(),
            "built-in corpus should not be empty"
        );
        for page in &listed.pages {
            get_page(None, &page.id)
                .unwrap_or_else(|e| panic!("indexed page '{}' failed to load: {e}", page.id));
        }
    }

    /// Every page names a canonical human doc, and that doc exists.
    ///
    /// Seven wiki pages share a basename with a doc under `docs/`. That is the
    /// legitimate duplication in this repo — a wiki page is a digest served
    /// into an agent's context, the doc is the human reference — but with
    /// nothing recording which is authoritative, both drifted. One of them
    /// drifted into stating the wrong active constitution version to every
    /// agent that read it.
    ///
    /// `canonical` lives in `index.toml`, not in the page body, so it costs no
    /// prompt tokens: the index is parsed, never served.
    #[test]
    fn every_page_names_a_canonical_doc_that_exists() {
        let dir = resolve_wiki_dir(None).expect("built-in corpus dir should exist");
        let index = load_index(&dir).expect("index should parse");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace parent");

        for page in &index.pages {
            let canonical = page.canonical.as_deref().unwrap_or_else(|| {
                panic!(
                    "wiki page '{}' has no `canonical` in index.toml. Every page \
                     digests some human doc — name it, so the two cannot drift \
                     apart unnoticed.",
                    page.id
                )
            });
            assert!(
                root.join(canonical).exists(),
                "wiki page '{}' names canonical doc '{}', which does not exist",
                page.id,
                canonical
            );
        }
    }

    /// A wiki page is a digest, and a digest has a size bound.
    ///
    /// Without one, a page grows until it is a second reference — at which
    /// point it is both duplicated and paid for in every agent's context. The
    /// ceiling is generous against today's corpus (largest page 162 lines);
    /// hitting it means the material belongs in the canonical doc, with the
    /// page pointing at it.
    #[test]
    fn wiki_pages_stay_digest_sized() {
        const MAX_LINES: usize = 200;
        let dir = resolve_wiki_dir(None).expect("built-in corpus dir should exist");
        let index = load_index(&dir).expect("index should parse");

        let mut oversized = Vec::new();
        for page in &index.pages {
            let path = dir.join(&page.file);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let lines = text.lines().count();
            if lines > MAX_LINES {
                oversized.push(format!(
                    "{} ({} lines, budget {MAX_LINES}) — canonical: {}",
                    page.file,
                    lines,
                    page.canonical.as_deref().unwrap_or("(none)")
                ));
            }
        }

        assert!(
            oversized.is_empty(),
            "{} wiki page(s) over the digest budget. A page this long is a \
             second reference doc, duplicated and charged to every agent's \
             context — move the detail into the canonical doc and point at \
             it.\n\n  {}\n",
            oversized.len(),
            oversized.join("\n  ")
        );
    }

    /// Conversely, every `.md` file in the corpus must be indexed — an
    /// unindexed page is invisible to `wiki_list`, so agents never find it.
    ///
    /// Matched on the index's `file` field, not on `id`. The two are separate
    /// columns and the format does not require them to agree, so comparing
    /// filenames against ids would enforce a convention the data model does not
    /// have and fail on a page that legitimately names them differently.
    #[test]
    fn every_corpus_file_is_indexed() {
        let dir = resolve_wiki_dir(None).expect("built-in corpus dir should exist");
        let index = load_index(&dir).expect("index should parse");
        let indexed_files: std::collections::HashSet<&str> =
            index.pages.iter().map(|p| p.file.as_str()).collect();

        for entry in std::fs::read_dir(&dir).expect("corpus dir should read") {
            let entry = entry.expect("dir entry");
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".md") || name == "README.md" {
                continue; // README is authoring instructions, not a wiki page
            }
            assert!(
                indexed_files.contains(name.as_str()),
                "corpus file '{name}' has no `file = ` entry in index.toml, so no agent can discover it"
            );
        }
    }

    // ------------------------------------------------------------------
    // Config-citation drift guard
    // ------------------------------------------------------------------
    //
    // Wiki pages advise agents how to advise the *operator* about gateway
    // config. A page that hallucinates a key name (`stream_timeout`, …) is
    // worse than no page: every agent repeats the lie with confidence. Pages
    // therefore cite keys in a machine-checkable form — `` `config:a.b.c` ``
    // and `` `env:NAME` `` — and this test validates every citation:
    //
    // - `config:` paths against the serde field schema *parsed from
    //   `autonoetic-types/src/config.rs` at test time* (not a hand-maintained
    //   copy — a renamed field changes the source, and the test sees the new
    //   truth immediately);
    // - `env:` names against a source-tree scan (the literal must appear in
    //   `autonoetic-gateway/src/` or `autonoetic/src/`).
    //
    // Same contract as the enforcement register's
    // `every_parseable_citation_resolves`: a stale citation fails the build.

    /// One `pub struct X { ... }` from config.rs: field name → declared type
    /// text. Line-based parse — the config structs are flat (fields + doc
    /// comments, no nested items), closed by a `}` at column 0.
    fn parse_config_structs(
        src: &str,
    ) -> std::collections::HashMap<String, std::collections::HashMap<String, String>> {
        let mut structs = std::collections::HashMap::new();
        let mut lines = src.lines().peekable();
        while let Some(line) = lines.next() {
            let name = match line.trim_start().strip_prefix("pub struct ") {
                Some(rest) => rest.trim_end().trim_end_matches('{').trim().to_string(),
                None => continue,
            };
            let mut fields = std::collections::HashMap::new();
            for field_line in lines.by_ref() {
                let t = field_line.trim();
                if t == "}" {
                    break;
                }
                let Some(rest) = t.strip_prefix("pub ") else {
                    continue;
                };
                let Some((field, ty)) = rest.split_once(':') else {
                    continue;
                };
                let field = field.trim().to_string();
                let ty = ty.trim().trim_end_matches(',').to_string();
                if field.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !field.is_empty()
                {
                    fields.insert(field, ty);
                }
            }
            structs.insert(name, fields);
        }
        structs
    }

    /// Normalize a declared type for structural matching: strip the
    /// `std::collections::` qualification (config.rs mixes bare `HashMap<...>`
    /// and fully-qualified `std::collections::HashMap<...>`) and unwrap
    /// `Option<T>`.
    fn normalize_type(ty: &str) -> String {
        let ty = ty.strip_prefix("Option<").and_then(|t| t.strip_suffix('>')).unwrap_or(ty);
        let ty = ty.strip_prefix("std::collections::").unwrap_or(ty);
        ty.to_string()
    }

    /// Validate one dotted `config:` path against the parsed schema.
    /// `HashMap<String, X>` fields consume one free-form segment (the map
    /// key, e.g. a preset name) before descending into `X`; scalar/leaf
    /// types admit no further segments. A citation may *end* on a field of
    /// any type — enums (`SchemaEnforcementMode`) and externally-defined
    /// types (`crate::agent::ThinkingConfig`) are real config surface even
    /// though the parser can't see inside them — but cannot descend into
    /// one.
    fn check_config_path(
        path: &str,
        structs: &std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    ) -> Result<(), String> {
        let segments: Vec<&str> = path.split('.').collect();
        if segments.iter().any(|s| s.is_empty()) {
            return Err(format!("empty segment in '{path}'"));
        }
        let mut current = "GatewayConfig".to_string();
        let mut i = 0;
        while i < segments.len() {
            let seg = segments[i];
            let fields = structs
                .get(&current)
                .ok_or_else(|| format!("struct '{current}' not found in config.rs"))?;
            let ty = fields
                .get(seg)
                .cloned()
                .ok_or_else(|| format!("'{seg}' is not a field of {current}"))?;
            i += 1;
            let ty = normalize_type(&ty);
            let terminal = i >= segments.len();
            // A map consumes one free-form key segment, then validates
            // against the value type. A terminal citation on the map (or on
            // its `<key>`) is fine even when the value type is opaque.
            if let Some(value_ty) = ty
                .strip_prefix("HashMap<String, ")
                .and_then(|t| t.strip_suffix('>'))
            {
                if i < segments.len() {
                    i += 1; // the map key (e.g. the preset name)
                }
                let value_ty = normalize_type(value_ty);
                if is_leaf_type(&value_ty) {
                    if i < segments.len() {
                        return Err(format!(
                            "'{path}': '{}' is a map to a scalar; no segments after '{} <key>'",
                            value_ty,
                            segments[i - 2]
                        ));
                    }
                    return Ok(());
                }
                if i >= segments.len() {
                    return Ok(()); // ends on the map's value — real surface
                }
                if !structs.contains_key(&value_ty) {
                    return Err(format!(
                        "'{path}': map value type '{value_ty}' is not a known config struct; \
                         cannot descend into '{}'",
                        segments[i]
                    ));
                }
                current = value_ty;
                continue;
            }
            if is_leaf_type(&ty) {
                if !terminal {
                    return Err(format!(
                        "'{path}': '{}' is a scalar ({}); no further segments",
                        seg,
                        ty
                    ));
                }
                return Ok(());
            }
            if !structs.contains_key(&ty) {
                // Unknown interior type (enum, or a type defined outside
                // config.rs): citable as a terminal field, not descendable.
                if terminal {
                    return Ok(());
                }
                return Err(format!(
                    "'{path}': field type '{ty}' is opaque to the schema parser; \
                     cannot descend into '{seg}'"
                ));
            }
            if terminal {
                return Ok(()); // ends on a struct-valued field — citable
            }
            current = ty;
        }
        Ok(())
    }

    fn is_leaf_type(ty: &str) -> bool {
        let scalars = [
            "String", "bool", "u8", "u16", "u32", "u64", "usize", "i32", "i64", "f32", "f64",
            "PathBuf",
        ];
        scalars.contains(&ty)
            || ty.starts_with("Vec<")
            || ty.starts_with("HashMap<String, String")
    }

    /// Extract every `` `config:...` `` / `` `env:...` `` citation from a
    /// corpus page.
    fn extract_citations(content: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for token in content.split('`') {
            let token = token.trim();
            if let Some(path) = token.strip_prefix("config:") {
                out.push(("config".to_string(), path.trim().to_string()));
            } else if let Some(name) = token.strip_prefix("env:") {
                out.push(("env".to_string(), name.trim().to_string()));
            }
        }
        out
    }

    #[test]
    fn config_citations_in_corpus_resolve_against_gateway_config_schema() {
        let structs = parsed_config_structs_for_tests();

        let dir = resolve_wiki_dir(None).expect("built-in corpus dir should exist");
        let index = load_index(&dir).expect("index should parse");
        for page in &index.pages {
            let content =
                std::fs::read_to_string(dir.join(&page.file)).expect("corpus page should read");
            for (kind, cite) in extract_citations(&content) {
                if kind == "config" {
                    // `<name>` is a placeholder for a user-chosen map key
                    // (e.g. the preset name in llm_presets.<name>.model).
                    let normalized = cite.replace("<name>", "probe");
                    check_config_path(&normalized, &structs).unwrap_or_else(|e| {
                        panic!("page '{}': config citation {e} — the key was renamed/removed, update the page", page.id)
                    });
                }
            }
        }
    }

    fn parsed_config_structs_for_tests(
    ) -> std::collections::HashMap<String, std::collections::HashMap<String, String>> {
        let config_src_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../autonoetic-types/src/config.rs");
        let config_src = std::fs::read_to_string(&config_src_path).unwrap_or_else(|e| {
            panic!("cannot read {} for the citation guard: {e}", config_src_path.display())
        });
        let structs = parse_config_structs(&config_src);
        assert!(
            structs.contains_key("GatewayConfig"),
            "config.rs parse must find GatewayConfig — the drift guard is broken"
        );
        structs
    }

    /// PR-review regressions for the path checker itself, against the live
    /// parsed schema (so a schema change that breaks an assumption here also
    /// breaks this test, not silently the guard):
    /// - a citation may END on an enum/externally-typed field (real key,
    ///   opaque interior) but not descend into one;
    /// - fully-qualified `std::collections::HashMap<...>` field types match
    ///   the same as bare `HashMap<...>`.
    #[test]
    fn config_path_checker_terminal_opaque_types_and_qualified_maps() {
        let structs = parsed_config_structs_for_tests();
        // Enum-typed scalar: citable as a terminal, not descendable.
        assert!(check_config_path("schema_enforcement.mode", &structs).is_ok());
        assert!(check_config_path("schema_enforcement.mode.bogus", &structs).is_err());
        // Fully-qualified map (agent_overrides: std::collections::HashMap<String,
        // SchemaEnforcementMode>): key-segment handling identical to bare maps.
        assert!(check_config_path("schema_enforcement.agent_overrides", &structs).is_ok());
        assert!(check_config_path("schema_enforcement.agent_overrides.<k>", &structs).is_ok());
        assert!(check_config_path("schema_enforcement.agent_overrides.<k>.x", &structs).is_err());
        // Externally-defined option type: terminal OK, descend Err.
        assert!(check_config_path("llm_presets.<n>.thinking", &structs).is_ok());
        assert!(check_config_path("llm_presets.<n>.thinking.x", &structs).is_err());
        // Struct-typed field: citable bare AND descendable.
        assert!(check_config_path("loop_guard", &structs).is_ok());
        assert!(check_config_path("loop_guard.max_tool_failures", &structs).is_ok());
        // Sanity: the incident's hallucinated key still fails.
        assert!(check_config_path("stream_timeout", &structs).is_err());
    }

    #[test]
    fn env_citations_in_corpus_resolve_in_source() {
        let dir = resolve_wiki_dir(None).expect("built-in corpus dir should exist");
        let index = load_index(&dir).expect("index should parse");
        let mut cited: Vec<String> = Vec::new();
        for page in &index.pages {
            let content =
                std::fs::read_to_string(dir.join(&page.file)).expect("corpus page should read");
            for (kind, cite) in extract_citations(&content) {
                if kind == "env" {
                    cited.push(cite);
                }
            }
        }
        cited.sort();
        cited.dedup();
        if cited.is_empty() {
            return;
        }
        // One scan of the two source trees; every cited literal must occur.
        let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut haystack = String::new();
        for root in [
            crate_dir.join("src"),
            crate_dir.join("../autonoetic/src"),
        ] {
            for entry in walkdir::WalkDir::new(&root)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
            {
                if let Ok(text) = std::fs::read_to_string(entry.path()) {
                    haystack.push_str(&text);
                }
            }
        }
        for name in &cited {
            assert!(
                haystack.contains(name.as_str()),
                "env citation '{name}' appears in a wiki page but nowhere in \
                 autonoetic-gateway/src or autonoetic/src — the var was renamed/removed, \
                 update the page"
            );
        }
    }
}
