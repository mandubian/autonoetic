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
    registry.register(Box::new(ArtifactBuildTool));
    registry.register(Box::new(ArtifactInspectTool));
    registry.register(Box::new(ArtifactResolveRefTool));
}

fn mint_artifact_ref_id() -> String {
    let b = *uuid::Uuid::new_v4().as_bytes();
    format!(
        "ar.{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5]
    )
}

pub struct ArtifactBuildTool;

impl NativeTool for ArtifactBuildTool {
    fn name(&self) -> &'static str {
        "artifact_build"
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
            description: "Build an immutable artifact bundle from session content. Returns an artifact ID for review/install/closed-boundary execution. Artifacts are specialist-boundary objects: use them for evaluation, installation, and reproducible execution. For ordinary parent-child output handoff, prefer the implicit output from workflow.wait instead.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "inputs": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of content names or handles to include in the artifact"
                    },
                    "entrypoints": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of entrypoint filenames (must be in inputs)"
                    },
                    "layers": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "layer_id": { "type": "string" },
                                "name": { "type": "string" },
                                "mount_path": { "type": "string" },
                                "digest": { "type": "string" }
                            },
                            "required": ["layer_id", "name", "mount_path", "digest"]
                        },
                        "description": "Optional list of layer references to include in the artifact"
                    },
                    "kind": {
                        "type": "string",
                        "enum": ["binary", "skill_bundle", "agent_bundle", "dataset", "gateway_runtime", "report"],
                        "description": "Optional artifact kind for downstream policy checks. Defaults to 'binary'."
                    }
                },
                "required": ["inputs"],
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
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            inputs: Vec<String>,
            entrypoints: Option<Vec<String>>,
            layers: Option<Vec<autonoetic_types::layer::ArtifactLayer>>,
            kind: Option<String>,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(gw_dir) = gateway_dir else {
            return Ok(ToolError::resource("Artifact store requires gateway directory to be configured", None::<String>).to_error_response());
        };

        let sid = _session_id.unwrap_or(&_manifest.agent.id);
        let store = crate::artifact_store::ArtifactStore::new(gw_dir)?;

        if let Some(ref layers) = args.layers {
            let layer_store = crate::layer_store::LayerStore::new(gw_dir, Default::default())?;
            for layer in layers {
                let manifest = layer_store.inspect(&layer.layer_id).map_err(|_| {
                    anyhow::anyhow!(
                        "Layer '{}' referenced in artifact.build does not exist in layer store",
                        layer.layer_id
                    )
                })?;
                if manifest.digest != layer.digest {
                    return Ok(ToolError::fatal(
                        format!(
                            "Layer digest mismatch for '{}': artifact.build references digest '{}' but layer store has '{}'",
                            layer.layer_id,
                            layer.digest,
                            manifest.digest
                        ),
                        None::<String>,
                    ).to_error_response());
                }
            }
        }

        let raw_kind = args.kind.clone();
        let explicit_kind = raw_kind
            .as_deref()
            .map(|raw| {
                serde_json::from_value::<autonoetic_types::artifact::ArtifactKind>(
                    serde_json::Value::String(raw.to_string()),
                )
            })
            .transpose()
            .map_err(|_| {
                anyhow::anyhow!("Invalid artifact kind '{}'", raw_kind.unwrap_or_default())
            })?;

        let kind = if let Some(k) = explicit_kind {
            k
        } else {
            let mut inherited: Option<autonoetic_types::artifact::ArtifactKind> = None;
            for input in &args.inputs {
                if input.starts_with("art_") {
                    if let Ok(bundle) = store.inspect(input) {
                        if bundle.kind != autonoetic_types::artifact::ArtifactKind::Binary {
                            inherited = Some(bundle.kind.clone());
                            break;
                        }
                    }
                }
            }
            inherited.unwrap_or(autonoetic_types::artifact::ArtifactKind::Binary)
        };

        let bundle = store.build_with_kind(
            &args.inputs,
            args.entrypoints.as_deref(),
            args.layers.as_deref(),
            kind.clone(),
            sid,
        )?;

        let root = crate::runtime::content_store::root_session_id(sid);
        let (scope_type, scope_id) = match config {
            Some(cfg) => {
                match crate::scheduler::workflow_store::resolve_workflow_id_for_root_session(
                    cfg, root,
                ) {
                    Ok(Some(wf_id)) => (
                        autonoetic_types::artifact::ArtifactRefScopeType::Workflow,
                        wf_id,
                    ),
                    _ => (
                        autonoetic_types::artifact::ArtifactRefScopeType::Session,
                        sid.to_string(),
                    ),
                }
            }
            None => (
                autonoetic_types::artifact::ArtifactRefScopeType::Session,
                sid.to_string(),
            ),
        };

        let mut artifact_ref: Option<String> = None;
        let mut artifact_ref_scope: Option<serde_json::Value> = None;
        if let Some(gs) = gateway_store.as_ref() {
            let existing_ref = if bundle.reused {
                gs.list_artifact_refs_for_scope(scope_type, &scope_id)?
                    .into_iter()
                    .find(|record| {
                        record.artifact_id == bundle.artifact_id
                            && record.artifact_manifest_digest == bundle.artifact_manifest_digest
                    })
            } else {
                None
            };

            if let Some(record) = existing_ref {
                artifact_ref = Some(record.ref_id);
                artifact_ref_scope = Some(serde_json::json!({
                    "type": record.scope_type.as_str(),
                    "id": record.scope_id,
                }));
            } else {
                let ref_id = mint_artifact_ref_id();
                gs.create_artifact_ref(&autonoetic_types::artifact::ArtifactRefRecord {
                    ref_id: ref_id.clone(),
                    scope_type,
                    scope_id: scope_id.clone(),
                    artifact_id: bundle.artifact_id.clone(),
                    artifact_manifest_digest: bundle.artifact_manifest_digest.clone(),
                    artifact_canonical_digest: bundle.artifact_canonical_digest.clone(),
                    created_by_agent_id: _manifest.agent.id.clone(),
                    created_at: chrono::Utc::now().to_rfc3339(),
                    expires_at: None,
                    revoked_at: None,
                })?;
                artifact_ref = Some(ref_id);
                artifact_ref_scope = Some(serde_json::json!({
                    "type": scope_type.as_str(),
                    "id": scope_id,
                }));
            }
        }

        let mut out = serde_json::json!({
            "ok": true,
            "artifact_canonical_digest": bundle.artifact_canonical_digest,
            "artifact_manifest_digest": bundle.artifact_manifest_digest,
            "kind": serde_json::to_value(&bundle.kind)
                .unwrap_or(serde_json::Value::String("binary".to_string())),
            "files": bundle.files.iter().map(|f| serde_json::json!({
                "name": f.name,
                "handle": f.handle,
                "alias": f.alias,
            })).collect::<Vec<_>>(),
            "entrypoints": bundle.entrypoints,
            "created_at": bundle.created_at,
            "reused": bundle.reused,
            "message": if bundle.reused {
                "Reused existing artifact with same inputs"
            } else {
                "Created new artifact"
            }
        });
        if let (Some(r), Some(scope)) = (artifact_ref, artifact_ref_scope) {
            if let Some(obj) = out.as_object_mut() {
                obj.insert("artifact_ref".to_string(), serde_json::Value::String(r));
                obj.insert("artifact_ref_scope".to_string(), scope);
            }
        }
        serde_json::to_string(&out).map_err(Into::into)
    }

    fn extract_metadata(&self, arguments_json: &str) -> ToolMetadata {
        let mut meta = ToolMetadata::default();
        if let Ok(parsed_args) = serde_json::from_str::<serde_json::Value>(arguments_json) {
            if let Some(inputs) = parsed_args.get("inputs").and_then(|v| v.as_array()) {
                if let Some(first) = inputs.first().and_then(|v| v.as_str()) {
                    meta.path = Some(first.to_string());
                }
            }
        }
        meta
    }
}

pub struct ArtifactInspectTool;

impl NativeTool for ArtifactInspectTool {
    fn name(&self) -> &'static str {
        "artifact_inspect"
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
            description: "Inspect an artifact by its scoped ref. Returns file list, entrypoints, layers, digests, and metadata. Use this for specialist-boundary review (evaluation, audit, installation). Pass the artifact_ref returned by artifact_build or received from a child task.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "artifact_ref": {
                        "type": "string",
                        "description": "The artifact ref to inspect (e.g., 'ar.aabb1234ef56')"
                    }
                },
                "required": ["artifact_ref"],
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
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            artifact_ref: String,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(gw_dir) = gateway_dir else {
            return Ok(ToolError::resource("Artifact store requires gateway directory to be configured", None::<String>).to_error_response());
        };
        let Some(gs) = gateway_store else {
            return Ok(ToolError::resource("artifact_inspect requires GatewayStore to be configured", None::<String>).to_error_response());
        };
        let sid = session_id.unwrap_or_default();

        let ref_record = gs
            .resolve_artifact_ref_any_scope(&args.artifact_ref, sid)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "artifact_ref '{}' not found, expired, or revoked",
                    args.artifact_ref
                )
            })?;

        let store = crate::artifact_store::ArtifactStore::new(gw_dir)?;
        let bundle = store.inspect(&ref_record.artifact_id)?;

        if bundle.artifact_manifest_digest != ref_record.artifact_manifest_digest {
            return Ok(ToolError::fatal(
                format!(
                    "artifact_ref '{}' digest mismatch — possible tampering. Ref claims '{}', manifest has '{}'.",
                    args.artifact_ref,
                    ref_record.artifact_manifest_digest,
                    bundle.artifact_manifest_digest,
                ),
                None::<String>,
            ).to_error_response());
        }

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "artifact_ref": args.artifact_ref,
            "artifact_canonical_digest": bundle.artifact_canonical_digest,
            "artifact_manifest_digest": bundle.artifact_manifest_digest,
            "kind": serde_json::to_value(&bundle.kind)
                .unwrap_or(serde_json::Value::String("binary".to_string())),
            "files": bundle.files.iter().map(|f| serde_json::json!({
                "name": f.name,
                "alias": f.alias,
                "content_read_ref": format!("{}:{}", args.artifact_ref, f.name),
            })).collect::<Vec<_>>(),
            "layers": bundle.layers.iter().map(|l| serde_json::json!({
                "layer_id": l.layer_id,
                "name": l.name,
                "mount_path": l.mount_path,
                "digest": l.digest,
            })).collect::<Vec<_>>(),
            "entrypoints": bundle.entrypoints,
            "created_at": bundle.created_at,
        }))
        .map_err(Into::into)
    }
}

pub struct ArtifactResolveRefTool;

impl NativeTool for ArtifactResolveRefTool {
    fn name(&self) -> &'static str {
        "artifact_resolve_ref"
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
            description: "Resolve a short scoped artifact reference to its canonical artifact identity. Use this to inspect artifacts passed from child tasks without inlined file handles. Fails hard if the ref is missing, expired, revoked, or has a digest mismatch.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "ref_id": {
                        "type": "string",
                        "description": "The short artifact reference ID (e.g., 'ar.wf9f3.004.k7p2')"
                    },
                    "scope_type": {
                        "type": "string",
                        "enum": ["session", "workflow", "global"],
                        "description": "The scope namespace: 'session', 'workflow', or 'global'"
                    },
                    "scope_id": {
                        "type": "string",
                        "description": "The scope ID: session_id, workflow_id, or '__global__'"
                    }
                },
                "required": ["ref_id", "scope_type", "scope_id"],
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
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            ref_id: String,
            scope_type: String,
            scope_id: String,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.ref_id.trim().is_empty(), "ref_id must not be empty");
        anyhow::ensure!(
            !args.scope_id.trim().is_empty(),
            "scope_id must not be empty"
        );

        let scope_type =
            autonoetic_types::artifact::ArtifactRefScopeType::from_str(&args.scope_type)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Invalid scope_type '{}'. Must be 'session', 'workflow', or 'global'.",
                        args.scope_type
                    )
                })?;

        let Some(store) = gateway_store else {
            return Ok(ToolError::resource("artifact.resolve_ref requires GatewayStore to be configured", None::<String>).to_error_response());
        };

        let Some(ref_record) =
            store.resolve_artifact_ref(scope_type, &args.scope_id, &args.ref_id)?
        else {
            return Err(anyhow::Error::from(
                autonoetic_types::tool_error::tagged::Tagged::validation(anyhow::anyhow!(
                    "Artifact ref '{}' not found in {} scope '{}', or it is expired/revoked.",
                    args.ref_id,
                    scope_type.as_str(),
                    args.scope_id
                )),
            )
            .into());
        };

        let Some(gw_dir) = gateway_dir else {
            return Ok(ToolError::resource("artifact.resolve_ref requires gateway directory to be configured", None::<String>).to_error_response());
        };

        let artifact_store = crate::artifact_store::ArtifactStore::new(gw_dir)?;
        let bundle = artifact_store.inspect(&ref_record.artifact_id)?;

        if bundle.artifact_manifest_digest != ref_record.artifact_manifest_digest {
            return Err(anyhow::Error::from(autonoetic_types::tool_error::tagged::Tagged::validation(
                anyhow::anyhow!(
                    "Artifact digest mismatch for ref '{}'. Ref claims '{}' but artifact manifest has '{}'. Possible tampering or corruption.",
                    args.ref_id,
                    ref_record.artifact_manifest_digest,
                    bundle.artifact_manifest_digest
                )
            )).into());
        }

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "artifact_ref": args.ref_id,
            "artifact_canonical_digest": bundle.artifact_canonical_digest,
            "artifact_manifest_digest": bundle.artifact_manifest_digest,
            "kind": serde_json::to_value(&bundle.kind)
                .unwrap_or(serde_json::Value::String("binary".to_string())),
            "files": bundle.files.iter().map(|f| serde_json::json!({
                "name": f.name,
                "alias": f.alias,
                "content_read_ref": format!("{}:{}", args.ref_id, f.name),
            })).collect::<Vec<_>>(),
            "entrypoints": bundle.entrypoints,
            "created_at": bundle.created_at,
            "ref_created_at": ref_record.created_at,
            "ref_created_by": ref_record.created_by_agent_id,
        }))
        .map_err(Into::into)
    }
}
