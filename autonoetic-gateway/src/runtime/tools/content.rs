use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry, ToolMetadata};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(ContentWriteTool));
}

pub struct ContentWriteTool;

impl NativeTool for ContentWriteTool {
    fn name(&self) -> &'static str {
        "content_write"
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
            description: "Write content to the session's content store. Returns the registered name, a short ref (`cnt_<8 hex>`), and `sandbox_path` (`/tmp/<name>`) for sandbox.exec. Use `content.read` with the name, 8-char alias, or ref — not the digest as a shell path. Optional `include_canonical_digest` adds the sha256 digest for debugging.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "A name for this content (e.g., 'main.py', 'scripts/main.py'). Supports path-like names with slashes."
                    },
                    "content": {
                        "type": "string",
                        "description": "The content to store"
                    },
                    "visibility": {
                        "type": "string",
                        "enum": ["private", "session", "global"],
                        "description": "Visibility scope: 'private' (only this session), 'session' (all sessions under same root, default), 'global' (cross-session)."
                    },
                    "include_canonical_digest": {
                        "type": "boolean",
                        "description": "If true, include `canonical_digest` (sha256:...) in the response. Default false — omit to save tokens and avoid models misusing digests as paths."
                    }
                },
                "required": ["name", "content"],
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
        _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            name: String,
            #[serde(deserialize_with = "crate::runtime::tools::deserialize_string_lenient")]
            content: String,
            #[serde(default)]
            visibility: Option<String>,
            #[serde(default)]
            include_canonical_digest: bool,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.name.trim().is_empty(), "name must not be empty");
        anyhow::ensure!(
            args.name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/'),
            "name must contain only alphanumeric characters, underscores, hyphens, dots, or slashes"
        );

        let content_visibility = match args.visibility.as_deref() {
            Some("private") => crate::runtime::content_store::ContentVisibility::Private,
            Some("session") | None => crate::runtime::content_store::ContentVisibility::Session,
            Some("global") => crate::runtime::content_store::ContentVisibility::Global,
            Some(other) => {
                return Ok(ToolError::validation(
                    format!("Invalid visibility '{}'. Must be one of: private, session, global", other),
                    None::<String>,
                ).to_error_response());
            }
        };

        let Some(gw_dir) = gateway_dir else {
            return Ok(ToolError::resource("Content store requires gateway directory to be configured", None::<String>).to_error_response());
        };

        let sid = _session_id.unwrap_or(&_manifest.agent.id);
        let store = crate::runtime::content_store::ContentStore::new(gw_dir)?;

        let handle = store.write(args.content.as_bytes())?;
        store.register_name_with_visibility(sid, &args.name, &handle, content_visibility)?;

        let short_alias = crate::runtime::content_store::ContentStore::get_short_alias(&handle);
        let content_ref = format!("cnt_{}", short_alias);
        let sandbox_path = format!("/tmp/{}", args.name);

        let mut out = serde_json::json!({
            "ok": true,
            "name": args.name,
            "alias": short_alias,
            "ref": content_ref,
            "sandbox_path": sandbox_path,
            "bytes_written": args.content.len(),
            "visibility": match content_visibility {
                crate::runtime::content_store::ContentVisibility::Private => "private",
                crate::runtime::content_store::ContentVisibility::Session => "session",
                crate::runtime::content_store::ContentVisibility::Global => "global",
            },
        });
        if args.include_canonical_digest {
            out["canonical_digest"] = serde_json::Value::String(handle);
        }

        serde_json::to_string(&out).map_err(Into::into)
    }

    fn extract_metadata(&self, arguments_json: &str) -> ToolMetadata {
        let mut meta = ToolMetadata::default();
        if let Ok(parsed_args) = serde_json::from_str::<serde_json::Value>(arguments_json) {
            if let Some(name) = parsed_args.get("name").and_then(|v| v.as_str()) {
                meta.path = Some(name.to_string());
            }
        }
        meta
    }
}

pub(crate) fn find_available_artifacts(
    _store: &crate::runtime::content_store::ContentStore,
    _session_id: &str,
    _requested_name: &str,
) -> Vec<serde_json::Value> {
    let mut hints = Vec::new();

    hints.push(serde_json::json!({
        "suggestion": "Use workflow.wait or workflow.state to get stable output handles from completed child tasks. Succeeded tasks include an 'output' field with named_outputs and artifacts[].artifact_ref.",
        "example": "Call workflow.state first, then read completed_tasks[].output via content.read using named_outputs[*].ref or ar.<ref>:<filename>."
    }));

    hints
}

pub(crate) fn skill_path_repair_hint(gateway_dir: &Path, input: &str) -> Option<String> {
    let trimmed = input.trim();
    let normalized = trimmed.trim_start_matches("./");
    let looks_like_skill_path = normalized.starts_with("skills/") || trimmed.ends_with("/SKILL.md");

    if looks_like_skill_path {
        let suggested = normalized.strip_prefix("skills/").map_or_else(
            || normalized.to_string(),
            |_| normalized.to_string(),
        );
        let candidate = gateway_dir.join(&suggested);
        let suffix = if candidate.is_file() {
            format!(" Use `credential_setup` with `skill_url: \"{}\"`.", suggested)
        } else {
            " If you mean a gateway skill file, use `credential_setup` with `skill_url: \"skills/<service>/SKILL.md\"`.".to_string()
        };
        return Some(format!(
            "resolve only reads session content names/handles, not gateway-local skill files.{} If the skill was fetched into the session, read the stored content name or handle instead.",
            suffix
        ));
    }

    if trimmed == "SKILL.md" {
        return Some(
            "resolve only reads session content names/handles, not gateway-local skill files. If you mean a gateway skill file, use `credential_setup` with `skill_url: \"skills/<service>/SKILL.md\"`. If the skill was fetched into the session, read the stored content name or handle instead.".to_string(),
        );
    }

    None
}

pub(crate) fn try_read_artifact_ref_file(
    gw_dir: &Path,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    name_or_handle: &str,
    session_id: &str,
) -> anyhow::Result<Vec<u8>> {
    // Format: ar.<ref_id>:<filename>
    if !name_or_handle.starts_with("ar.") {
        return Err(autonoetic_types::tool_error::tagged::Tagged::validation(anyhow::anyhow!("not an artifact ref")).into());
    }

    let Some(colon_idx) = name_or_handle.find(':') else {
        return Err(autonoetic_types::tool_error::tagged::Tagged::validation(anyhow::anyhow!(
            "artifact ref must be in format `ar.<ref>:<filename>` (colon separator required)"
        )).into());
    };
    let ref_id = &name_or_handle[..colon_idx];
    let filename = &name_or_handle[colon_idx + 1..];

    let gs = gateway_store
        .ok_or_else(|| anyhow::anyhow!("GatewayStore required to resolve artifact refs"))?;

    let resolved =
        crate::runtime::tools::artifact::resolve_artifact_ref_or_canonical(
            ref_id,
            session_id,
            &gs,
            gw_dir,
        )?;

    let artifact_store = crate::artifact_store::ArtifactStore::new(gw_dir)?;
    let bundle = artifact_store
        .inspect(&resolved.artifact_id)
        .map_err(|e| anyhow::anyhow!("artifact '{}' not found: {}", resolved.artifact_id, e))?;

    let file_entry = bundle
        .files
        .iter()
        .find(|f| f.name == filename)
        .ok_or_else(|| {
            let available: Vec<&str> = bundle.files.iter().map(|f| f.name.as_str()).collect();
            anyhow::anyhow!(
                "file '{}' not found in artifact '{}'. Available files: {:?}",
                filename,
                resolved.artifact_id,
                available
            )
        })?;

    artifact_store
        .content_store()
        .read(&file_entry.handle)
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to read content for file '{}' in artifact '{}': {}",
                filename,
                resolved.artifact_id,
                e
            )
        })
}
