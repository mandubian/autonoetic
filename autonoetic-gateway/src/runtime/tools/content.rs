use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry, ToolMetadata};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use serde::Deserialize;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(ContentWriteTool));
    registry.register(Box::new(ContentReadTool));
}

pub struct ContentWriteTool;

impl NativeTool for ContentWriteTool {
    fn name(&self) -> &'static str {
        "content.write"
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
            Some(other) => anyhow::bail!(
                "Invalid visibility '{}'. Must be one of: private, session, global",
                other
            ),
        };

        let Some(gw_dir) = gateway_dir else {
            anyhow::bail!("Content store requires gateway directory to be configured");
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

pub struct ContentReadTool;

impl NativeTool for ContentReadTool {
    fn name(&self) -> &'static str {
        "content.read"
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
            description: "Read content from the session's content store. Prefer `name`, 8-char `alias`, or `cnt_<alias>` ref from content.write. Also supports `art_<id>:<filename>` or `art_<id>/<filename>` to read a specific file from an artifact. Full `sha256:...` digest still works for backward compatibility.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name_or_handle": {
                        "type": "string",
                        "description": "Content name (e.g. 'main.py'), 8-hex alias, cnt_<alias> ref, art_<id>:<filename>, or sha256 digest — not a sandbox path"
                    }
                },
                "required": ["name_or_handle"],
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
            name_or_handle: String,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(
            !args.name_or_handle.trim().is_empty(),
            "name_or_handle must not be empty"
        );

        let Some(gw_dir) = gateway_dir else {
            anyhow::bail!("Content store requires gateway directory to be configured");
        };

        let sid = _session_id.unwrap_or(&_manifest.agent.id);
        let store = crate::runtime::content_store::ContentStore::new(gw_dir)?;

        let input = args.name_or_handle.trim();

        let content = if input.starts_with("art_") {
            match try_read_artifact_file(gw_dir, input) {
                Ok(c) => c,
                Err(e) => {
                    anyhow::bail!(
                        "Content '{}' not found: {}. Use `artifact.inspect` to verify the artifact ID and file list. Supported formats: `art_<id>:<filename>` or `art_<id>/<filename>`.",
                        input,
                        e
                    );
                }
            }
        } else {
            match store.read_by_name_or_handle(sid, input) {
                Ok(c) => c,
                Err(e) => {
                    let looks_like_guessed_name = !input.starts_with("sha256:");

                    if looks_like_guessed_name {
                        let hints = find_available_artifacts(&store, sid, input);

                        if !hints.is_empty() {
                            return Ok(serde_json::json!({
                                "ok": false,
                                "error_type": "resource",
                                "error": "content_not_found",
                                "message": format!("Content '{}' not found in session '{}'", input, sid),
                                "hint": "Use workflow.wait or workflow.state to get stable output handles from completed child tasks, then use content.read with the artifact_id from the output field.",
                                "available_artifacts": hints
                            }).to_string());
                        }
                    }

                    anyhow::bail!("Content '{}' not found in session '{}': {}", input, sid, e);
                }
            }
        };

        let content_str = String::from_utf8(content)
            .map_err(|e| anyhow::anyhow!("Content is not valid UTF-8: {}", e))?;

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "content": content_str,
        }))
        .map_err(Into::into)
    }

    fn extract_metadata(&self, arguments_json: &str) -> ToolMetadata {
        let mut meta = ToolMetadata::default();
        if let Ok(parsed_args) = serde_json::from_str::<serde_json::Value>(arguments_json) {
            if let Some(name) = parsed_args.get("name_or_handle").and_then(|v| v.as_str()) {
                meta.path = Some(name.to_string());
            }
        }
        meta
    }
}

fn find_available_artifacts(
    _store: &crate::runtime::content_store::ContentStore,
    _session_id: &str,
    _requested_name: &str,
) -> Vec<serde_json::Value> {
    let mut hints = Vec::new();

    hints.push(serde_json::json!({
        "suggestion": "Use workflow.wait or workflow.state to get stable output handles from completed child tasks. Succeeded tasks include an 'output' field with an implicit artifact_id.",
        "example": "Call workflow.state first, then use the artifact_id from completed_tasks[].output to read the child's result."
    }));

    hints
}

fn try_read_artifact_file(gw_dir: &Path, name_or_handle: &str) -> anyhow::Result<Vec<u8>> {
    if !name_or_handle.starts_with("art_") {
        anyhow::bail!("not an artifact ref");
    }

    let (artifact_id, filename) = if let Some(idx) = name_or_handle.rfind(':') {
        (&name_or_handle[..idx], &name_or_handle[idx + 1..])
    } else if let Some(idx) = name_or_handle.find('/') {
        (&name_or_handle[..idx], &name_or_handle[idx + 1..])
    } else {
        anyhow::bail!(
            "artifact ref must be in format `art_<id>:<filename>` or `art_<id>/<filename>`"
        );
    };

    let artifact_store = crate::artifact_store::ArtifactStore::new(gw_dir)?;
    let bundle = artifact_store
        .inspect(artifact_id)
        .map_err(|e| anyhow::anyhow!("artifact '{}' not found: {}", artifact_id, e))?;

    let file_entry = bundle
        .files
        .iter()
        .find(|f| f.name == filename)
        .ok_or_else(|| {
            let available: Vec<&str> = bundle.files.iter().map(|f| f.name.as_str()).collect();
            anyhow::anyhow!(
                "file '{}' not found in artifact '{}'. Available files: {:?}",
                filename,
                artifact_id,
                available
            )
        })?;

    let content = artifact_store
        .content_store()
        .read(&file_entry.handle)
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to read content for file '{}' in artifact '{}': {}",
                filename,
                artifact_id,
                e
            )
        })?;

    Ok(content)
}
