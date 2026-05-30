//! `resolve` — the single front door for any artifact/content handle (#312).
//!
//! Agents juggle several handle shapes — `art_<id>`, `ar.<ref>`, `cnt_<alias>`,
//! a bare 8-char alias, a content `name`, and `sha256:…`.
//! Choosing *which tool* consumes *which shape* is exactly the decision that
//! drives tool-thrashing. `resolve` takes **any** of them and answers "what is
//! this / show me this" without that choice:
//!
//! - artifact-shaped refs (`art_` / `ar.`) → artifact resolution, with scope
//!   inferred from the session (no explicit `scope_type`/`scope_id`); a single
//!   file inside is selected with the `file` argument, not packed into the ref;
//! - everything else → the content store.
//!
//! The agent's decision collapses to: **run it → `artifact_exec`; see it →
//! `resolve`.** `resolve` is the sole read door — there is no separate
//! `content_read`; `artifact_inspect` remains for structural artifact review.

use std::path::Path;

use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use serde_json::json;

use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(ResolveTool));
}

pub struct ResolveTool;

impl NativeTool for ResolveTool {
    fn name(&self) -> &'static str {
        "resolve"
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
            description: "Resolve ANY artifact or content handle to what it points at — `art_`/`ar.` artifact refs, or `cnt_`/8-char alias/content name/`sha256:` content handles. The one front door for \"what is this / show me this\": you do not pick a tool by handle type. `include` controls depth: 'metadata' (default — identity + existence), 'files' (an artifact's file list), or 'content' (inline the bytes; for an artifact pass `file` to choose which file inside it). To RUN an artifact use artifact_exec; to SEE one use resolve."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "ref": { "type": "string", "description": "Any handle: art_<id>, ar.<ref>, cnt_<alias>, 8-char alias, content name, or sha256:…" },
                    "include": { "type": "string", "enum": ["metadata", "files", "content"], "description": "Depth: metadata (default), files (artifact file list), content (inline bytes)" },
                    "file": { "type": "string", "description": "For include=content on an artifact: which file inside it to read (the file name from include=files)" }
                },
                "required": ["ref"],
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
            #[serde(rename = "ref")]
            reference: String,
            include: Option<String>,
            file: Option<String>,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let reference = args.reference.trim();
        anyhow::ensure!(!reference.is_empty(), "ref must not be empty");
        let include = args.include.as_deref().unwrap_or("metadata");
        anyhow::ensure!(
            matches!(include, "metadata" | "files" | "content"),
            "include must be one of: metadata, files, content"
        );

        let Some(gw_dir) = gateway_dir else {
            return Ok(ToolError::resource(
                "resolve requires gateway directory to be configured",
                None::<String>,
            )
            .to_error_response());
        };
        let sid = session_id.unwrap_or(&manifest.agent.id);

        // Artifact-shaped refs (`art_` / `ar.`) take the artifact path. The
        // file selector is the `file` parameter — it is NOT packed into the
        // ref. Reject the legacy `ar.<ref>:<file>` packing with a nudge.
        if reference.starts_with("art_") || reference.starts_with("ar.") {
            if reference.contains(':') {
                return Ok(ToolError::validation(
                    "the file is selected with the `file` parameter, not packed into the ref — e.g. resolve(ref=\"ar.xxxx\", include=\"content\", file=\"foo.txt\")",
                    Some("Drop the ':<file>' suffix from ref and pass file=<name>."),
                )
                .to_error_response());
            }
            return self.resolve_artifact(
                gw_dir,
                gateway_store,
                sid,
                reference,
                include,
                args.file,
            );
        }

        self.resolve_content(gw_dir, sid, reference, include)
    }
}

impl ResolveTool {
    fn resolve_artifact(
        &self,
        gw_dir: &Path,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        sid: &str,
        artifact_ref: &str,
        include: &str,
        file: Option<String>,
    ) -> anyhow::Result<String> {
        let Some(gs) = gateway_store else {
            return Ok(ToolError::resource(
                "resolve requires GatewayStore to resolve artifact refs",
                None::<String>,
            )
            .to_error_response());
        };

        // `include=content` on an artifact reads a single named file.
        if include == "content" {
            let Some(file) = file else {
                return Ok(ToolError::validation(
                    "resolve(include=content) on an artifact needs a `file` (the file name inside it); use include=files to list them",
                    Some("Pass file=<name>, or call include=files first."),
                )
                .to_error_response());
            };
            return match crate::runtime::tools::content::read_artifact_file(
                gw_dir,
                Some(&gs),
                artifact_ref,
                &file,
                sid,
            ) {
                Ok(bytes) => {
                    let content = String::from_utf8(bytes)
                        .map_err(|e| anyhow::anyhow!("file is not valid UTF-8: {e}"))?;
                    Ok(json!({
                        "ok": true,
                        "kind": "artifact_file",
                        "ref": artifact_ref,
                        "file": file,
                        "content": content,
                    })
                    .to_string())
                }
                Err(e) => Ok(ToolError::not_found(
                    format!("file '{file}' in artifact '{artifact_ref}': {e}"),
                    Some("Use include=files to see the artifact's file list."),
                )
                .to_error_response()),
            };
        }

        let resolved = match crate::runtime::tools::artifact::resolve_artifact_ref_or_canonical(
            artifact_ref,
            sid,
            &gs,
            gw_dir,
        ) {
            Ok(r) => r,
            Err(e) => return Ok(ToolError::not_found(e.to_string(), None::<String>).to_error_response()),
        };

        let store = crate::artifact_store::ArtifactStore::new(gw_dir)?;
        let bundle = store.inspect(&resolved.artifact_id)?;

        // Tamper check, mirroring artifact_inspect.
        if let Some(ref ref_digest) = resolved.manifest_digest {
            if bundle.artifact_manifest_digest != *ref_digest {
                return Ok(ToolError::fatal(
                    format!(
                        "artifact_ref '{}' digest mismatch — possible tampering. Ref claims '{}', manifest has '{}'.",
                        artifact_ref, ref_digest, bundle.artifact_manifest_digest,
                    ),
                    None::<String>,
                )
                .to_error_response());
            }
        }

        let mut out = json!({
            "ok": true,
            "kind": "artifact",
            // Mirror artifact_inspect's identity fields (ref + digests) so
            // resolve(include=metadata) is a complete identity check. The raw
            // canonical `art_*` id is intentionally not surfaced — agents
            // address artifacts by `artifact_ref` (#312).
            "artifact_ref": resolved.display_ref,
            "artifact_canonical_digest": bundle.artifact_canonical_digest,
            "artifact_manifest_digest": bundle.artifact_manifest_digest,
            "exists": true,
            "artifact_kind": serde_json::to_value(&bundle.kind)
                .unwrap_or(serde_json::Value::String("binary".to_string())),
            "entrypoints": bundle.entrypoints,
            "file_count": bundle.files.len(),
            "created_at": bundle.created_at,
        });

        if include == "files" {
            if let Some(obj) = out.as_object_mut() {
                obj.insert(
                    "files".to_string(),
                    json!(bundle
                        .files
                        .iter()
                        .map(|f| json!({
                            "name": f.name,
                            "alias": f.alias,
                        }))
                        .collect::<Vec<_>>()),
                );
                obj.insert(
                    "read_file".to_string(),
                    json!(format!(
                        "resolve(ref=\"{}\", include=\"content\", file=<name>)",
                        resolved.display_ref
                    )),
                );
            }
        }
        Ok(out.to_string())
    }

    fn resolve_content(
        &self,
        gw_dir: &Path,
        sid: &str,
        reference: &str,
        include: &str,
    ) -> anyhow::Result<String> {
        let store = crate::runtime::content_store::ContentStore::new(gw_dir)?;

        if include == "content" {
            return match store.read_by_name_or_handle(sid, reference) {
                Ok(bytes) => {
                    let content = String::from_utf8(bytes)
                        .map_err(|e| anyhow::anyhow!("content is not valid UTF-8: {e}"))?;
                    Ok(json!({ "ok": true, "kind": "content", "ref": reference, "content": content })
                        .to_string())
                }
                Err(e) => Ok(self.content_not_found(gw_dir, &store, sid, reference, &e.to_string())),
            };
        }

        // metadata / files: existence without reading the blob.
        match store.resolve_name_or_handle_to_handle(sid, reference) {
            Ok(handle) => Ok(json!({
                "ok": true,
                "kind": "content",
                "ref": reference,
                "exists": true,
                "alias": crate::runtime::content_store::ContentStore::get_short_alias(&handle),
            })
            .to_string()),
            Err(e) => Ok(self.content_not_found(gw_dir, &store, sid, reference, &e.to_string())),
        }
    }

    /// Build a content not-found response, preserving the helpful hints the
    /// former `content_read` surfaced: a skills-path repair hint, or — when
    /// the agent likely guessed a name — the list of artifacts actually
    /// available in the session (anti-thrash, #312).
    fn content_not_found(
        &self,
        gw_dir: &Path,
        store: &crate::runtime::content_store::ContentStore,
        sid: &str,
        reference: &str,
        err: &str,
    ) -> String {
        if let Some(hint) =
            crate::runtime::tools::content::skill_path_repair_hint(gw_dir, reference)
        {
            return ToolError::not_found(
                format!("content '{reference}' not found in session '{sid}': {err}"),
                Some(hint),
            )
            .to_error_response();
        }
        if !reference.starts_with("sha256:") {
            let hints = crate::runtime::tools::content::find_available_artifacts(store, sid, reference);
            if !hints.is_empty() {
                return json!({
                    "ok": false,
                    "error_type": "resource",
                    "error": "content_not_found",
                    "message": format!("content '{reference}' not found in session '{sid}'"),
                    "repair_hint": "Use workflow.wait/workflow.state to get a stable output ref from a completed child, then resolve that.",
                    "available_artifacts": hints,
                })
                .to_string();
            }
        }
        ToolError::not_found(
            format!("content '{reference}' not found in session '{sid}': {err}"),
            None::<String>,
        )
        .to_error_response()
    }
}
