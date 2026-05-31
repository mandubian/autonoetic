use crate::artifact_store::ArtifactStore;
use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::content_store::ContentStore;
use crate::runtime::tools::{NativeTool, NativeToolRegistry, ToolMetadata};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::workbench::{
    FileChangeType, WorkbenchCheckpoint, WorkbenchFileDiff, WorkbenchProjection, WorkbenchStatus,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(ArtifactProjectTool));
    registry.register(Box::new(WorkbenchStatusTool));
    registry.register(Box::new(WorkbenchDiffTool));
    registry.register(Box::new(WorkbenchCheckpointTool));
    registry.register(Box::new(WorkbenchCheckpointsTool));
    registry.register(Box::new(WorkbenchCheckoutTool));
}

fn has_workbench_access(manifest: &AgentManifest) -> bool {
    manifest.capabilities.iter().any(|c| {
        matches!(c, Capability::PlanFrameAccess { .. })
    })
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn new_workbench_id() -> String {
    let bytes = uuid::Uuid::new_v4();
    format!("wb-{}", hex::encode(&bytes.as_bytes()[..6]))
}

fn new_checkpoint_id() -> String {
    let bytes = uuid::Uuid::new_v4();
    format!("cp-{}", hex::encode(&bytes.as_bytes()[..6]))
}

fn file_sha256(path: &Path) -> anyhow::Result<String> {
    let data = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

fn validate_relative_path(name: &str) -> anyhow::Result<()> {
    if name.contains("..") {
        anyhow::bail!("path traversal rejected: '{}'", name);
    }
    if name.starts_with('/') {
        anyhow::bail!("absolute path rejected: '{}'", name);
    }
    Ok(())
}

fn collect_workbench_files(source_dir: &Path) -> anyhow::Result<Vec<(String, u64)>> {
    let mut files = Vec::new();
    if !source_dir.exists() {
        return Ok(files);
    }
    for entry in walkdir::WalkDir::new(source_dir).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            let rel = entry.path().strip_prefix(source_dir).unwrap();
            let rel_str = rel.to_string_lossy().to_string();
            if rel_str.starts_with(".autonoetic/") {
                continue;
            }
            let metadata = std::fs::metadata(entry.path())?;
            files.push((rel_str, metadata.len()));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

pub struct ArtifactProjectTool;

impl NativeTool for ArtifactProjectTool {
    fn name(&self) -> &'static str {
        "artifact_project"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Project an artifact into an editable workbench directory. Files are copied (not symlinked) so the operator can edit them directly. The original artifact remains immutable.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "artifact_ref": {
                        "type": "string",
                        "description": "Artifact ref (ar.*) or artifact ID (art_*) to project"
                    },
                    "plan_id": {
                        "type": "string",
                        "description": "Optional plan_id to link this workbench to"
                    }
                },
                "required": ["artifact_ref"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        has_workbench_access(manifest)
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
        config: Option<&GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            artifact_ref: String,
            plan_id: Option<String>,
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(gateway_dir) = gateway_dir else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": "Gateway directory not available"
            }))?);
        };

        let Some(store) = gateway_store else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": "Gateway store not available"
            }))?);
        };

        let Some(config) = config else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": "Gateway config not available"
            }))?);
        };

        let session_id_val = session_id.ok_or_else(|| anyhow::anyhow!("session_id required"))?;
        let root_session_id = session_id_val.split('/').next().unwrap_or(session_id_val);

        let artifact_store = ArtifactStore::new(gateway_dir)?;

        let artifact_id = if args.artifact_ref.starts_with("art_") {
            args.artifact_ref.clone()
        } else {
            let resolved = crate::runtime::tools::artifact::resolve_artifact_ref_or_canonical(
                &args.artifact_ref,
                session_id_val,
                &store,
                gateway_dir,
            )?;
            resolved.artifact_id
        };

        let bundle = artifact_store.inspect(&artifact_id)?;

        for file in &bundle.files {
            validate_relative_path(&file.name)?;
        }

        let workflow = crate::scheduler::workflow_store::ensure_workflow_for_root_session(
            config,
            Some(&store),
            root_session_id,
            Some(&manifest.agent.id),
        )?;

        let workbench_id = new_workbench_id();
        let workbench_dir = gateway_dir.join("workbenches").join(&workbench_id);
        let source_dir = workbench_dir.join("source");
        let meta_dir = workbench_dir.join(".autonoetic");
        std::fs::create_dir_all(&source_dir)?;
        std::fs::create_dir_all(&meta_dir)?;

        let content_store = ContentStore::new(gateway_dir)?;

        for file in &bundle.files {
            validate_relative_path(&file.name)?;
            let output_path = source_dir.join(&file.name);
            if let Some(parent) = output_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let content = content_store.read(&file.handle)?;
            std::fs::write(&output_path, content)?;
        }

        let now = now_rfc3339();

        let projection_json = serde_json::json!({
            "workbench_id": workbench_id,
            "base_artifact_id": artifact_id,
            "base_artifact_canonical_digest": bundle.artifact_canonical_digest,
            "projected_at": now,
            "projected_by": manifest.agent.id,
        });
        std::fs::write(
            meta_dir.join("projection.json"),
            serde_json::to_string_pretty(&projection_json)?,
        )?;

        let base_digests: std::collections::HashMap<String, String> = bundle
            .files
            .iter()
            .map(|f| (f.name.clone(), f.handle.clone()))
            .collect();
        std::fs::write(
            meta_dir.join("base_digests.json"),
            serde_json::to_string_pretty(&base_digests)?,
        )?;

        let workspace_path = source_dir.to_string_lossy().to_string();

        let wb = WorkbenchProjection {
            workbench_id: workbench_id.clone(),
            workflow_id: workflow.workflow_id.clone(),
            root_session_id: root_session_id.to_string(),
            plan_id: args.plan_id,
            base_artifact_id: artifact_id.clone(),
            base_artifact_canonical_digest: bundle.artifact_canonical_digest.clone(),
            workspace_path: workspace_path.clone(),
            status: WorkbenchStatus::Active,
            created_by_agent_id: manifest.agent.id.clone(),
            created_at: now,
            last_checkpoint_at: None,
            reconciled_at: None,
            discarded_at: None,
        };

        store.save_workbench(&wb)?;

        let file_names: Vec<&str> = bundle.files.iter().map(|f| f.name.as_str()).collect();

        Ok(serde_json::to_string(&serde_json::json!({
            "ok": true,
            "workbench_id": workbench_id,
            "workspace_path": workspace_path,
            "artifact_id": artifact_id,
            "file_count": bundle.files.len(),
            "files": file_names,
            "message": "Artifact projected. Open the workspace path in any editor to make changes."
        }))?)
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}

pub struct WorkbenchStatusTool;

impl NativeTool for WorkbenchStatusTool {
    fn name(&self) -> &'static str {
        "workbench_status"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Get the status of a workbench, including file listing and modification state.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "workbench_id": {
                        "type": "string",
                        "description": "The workbench ID to inspect"
                    }
                },
                "required": ["workbench_id"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        has_workbench_access(manifest)
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
        _config: Option<&GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            workbench_id: String,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": "Gateway store not available"
            }))?);
        };

        let Some(wb) = store.load_workbench(&args.workbench_id)? else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": "Workbench not found"
            }))?);
        };

        let source_dir = Path::new(&wb.workspace_path);
        let files = collect_workbench_files(source_dir)?;

        let has_changes = if gateway_dir.is_some() && source_dir.exists() {
            let meta_dir = source_dir.parent().unwrap().join(".autonoetic");
            let digests_path = meta_dir.join("base_digests.json");
            if digests_path.exists() {
                let base_digests: std::collections::HashMap<String, String> =
                    serde_json::from_str(&std::fs::read_to_string(&digests_path)?)?;
                let mut changed = false;
                for (name, _) in &files {
                    if let Some(base) = base_digests.get(name) {
                        let current = file_sha256(&source_dir.join(name))?;
                        if &current != base {
                            changed = true;
                            break;
                        }
                    }
                }
                changed
            } else {
                false
            }
        } else {
            false
        };

        Ok(serde_json::to_string(&serde_json::json!({
            "ok": true,
            "workbench_id": wb.workbench_id,
            "status": wb.status.as_str(),
            "base_artifact_id": wb.base_artifact_id,
            "workspace_path": wb.workspace_path,
            "file_count": files.len(),
            "has_unsaved_changes": has_changes,
            "last_checkpoint_at": wb.last_checkpoint_at,
            "created_at": wb.created_at,
        }))?)
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}

pub struct WorkbenchDiffTool;

impl NativeTool for WorkbenchDiffTool {
    fn name(&self) -> &'static str {
        "workbench_diff"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Compare current workbench files against the base artifact. Returns a list of added, modified, deleted, and unchanged files.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "workbench_id": {
                        "type": "string",
                        "description": "The workbench ID to diff"
                    }
                },
                "required": ["workbench_id"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        has_workbench_access(manifest)
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            workbench_id: String,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": "Gateway store not available"
            }))?);
        };

        let Some(wb) = store.load_workbench(&args.workbench_id)? else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": "Workbench not found"
            }))?);
        };

        if wb.status != WorkbenchStatus::Active {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": format!("Workbench is in '{}' status", wb.status.as_str())
            }))?);
        }

        let source_dir = Path::new(&wb.workspace_path);
        if !source_dir.exists() {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": true, "diffs": [], "message": "Workspace directory does not exist"
            }))?);
        }

        let meta_dir = source_dir.parent().unwrap().join(".autonoetic");
        let digests_path = meta_dir.join("base_digests.json");
        let base_digests: std::collections::HashMap<String, String> = if digests_path.exists() {
            serde_json::from_str(&std::fs::read_to_string(&digests_path)?)?
        } else {
            std::collections::HashMap::new()
        };

        let current_files = collect_workbench_files(source_dir)?;
        let mut current_names: std::collections::HashSet<&str> =
            current_files.iter().map(|(n, _)| n.as_str()).collect();

        let mut diffs: Vec<WorkbenchFileDiff> = Vec::new();

        for (name, _) in &base_digests {
            if !current_names.contains(name.as_str()) {
                diffs.push(WorkbenchFileDiff {
                    path: name.clone(),
                    change_type: FileChangeType::Deleted,
                    base_digest: Some(base_digests[name].clone()),
                    current_digest: None,
                });
            }
        }

        for (name, _) in &current_files {
            match base_digests.get(name) {
                Some(base) => {
                    let current = file_sha256(&source_dir.join(name))?;
                    if &current == base {
                        diffs.push(WorkbenchFileDiff {
                            path: name.clone(),
                            change_type: FileChangeType::Unchanged,
                            base_digest: Some(base.clone()),
                            current_digest: Some(current),
                        });
                    } else {
                        diffs.push(WorkbenchFileDiff {
                            path: name.clone(),
                            change_type: FileChangeType::Modified,
                            base_digest: Some(base.clone()),
                            current_digest: Some(current),
                        });
                    }
                }
                None => {
                    let current = file_sha256(&source_dir.join(name))?;
                    diffs.push(WorkbenchFileDiff {
                        path: name.clone(),
                        change_type: FileChangeType::Added,
                        base_digest: None,
                        current_digest: Some(current),
                    });
                }
            }
        }

        diffs.sort_by(|a, b| a.path.cmp(&b.path));

        let changed = diffs.iter().filter(|d| d.change_type != FileChangeType::Unchanged).count();

        Ok(serde_json::to_string(&serde_json::json!({
            "ok": true,
            "workbench_id": wb.workbench_id,
            "base_artifact_id": wb.base_artifact_id,
            "total_files": diffs.len(),
            "changed_files": changed,
            "diffs": diffs,
        }))?)
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}

pub struct WorkbenchCheckpointTool;

impl NativeTool for WorkbenchCheckpointTool {
    fn name(&self) -> &'static str {
        "workbench_checkpoint"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Create a named checkpoint of the current workbench state. Files are snapshotted so they can be restored later.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "workbench_id": {
                        "type": "string",
                        "description": "The workbench ID to checkpoint"
                    },
                    "label": {
                        "type": "string",
                        "description": "Optional label for the checkpoint"
                    }
                },
                "required": ["workbench_id"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        has_workbench_access(manifest)
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            workbench_id: String,
            label: Option<String>,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": "Gateway store not available"
            }))?);
        };

        let Some(wb) = store.load_workbench(&args.workbench_id)? else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": "Workbench not found"
            }))?);
        };

        if wb.status != WorkbenchStatus::Active {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": format!("Cannot checkpoint a {} workbench", wb.status.as_str())
            }))?);
        }

        let source_dir = Path::new(&wb.workspace_path);
        let files = collect_workbench_files(source_dir)?;
        let total_bytes: u64 = files.iter().map(|(_, sz)| *sz).sum();

        let checkpoint_id = new_checkpoint_id();
        let now = now_rfc3339();

        let checkpoint_dir = source_dir
            .parent()
            .unwrap()
            .join(".autonoetic")
            .join("checkpoints")
            .join(&checkpoint_id);
        std::fs::create_dir_all(&checkpoint_dir)?;

        for (name, _) in &files {
            let src = source_dir.join(name);
            let dst = checkpoint_dir.join(name);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&src, &dst)?;
        }

        let cp = WorkbenchCheckpoint {
            checkpoint_id: checkpoint_id.clone(),
            workbench_id: wb.workbench_id.clone(),
            label: args.label,
            file_count: files.len(),
            total_bytes,
            created_at: now.clone(),
        };

        store.save_checkpoint(&cp)?;
        store.update_workbench_last_checkpoint(&wb.workbench_id, &now)?;

        Ok(serde_json::to_string(&serde_json::json!({
            "ok": true,
            "checkpoint_id": checkpoint_id,
            "workbench_id": wb.workbench_id,
            "file_count": files.len(),
            "total_bytes": total_bytes,
            "message": "Checkpoint created."
        }))?)
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}

pub struct WorkbenchCheckpointsTool;

impl NativeTool for WorkbenchCheckpointsTool {
    fn name(&self) -> &'static str {
        "workbench_checkpoints"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "List all checkpoints for a workbench.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "workbench_id": {
                        "type": "string",
                        "description": "The workbench ID"
                    }
                },
                "required": ["workbench_id"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        has_workbench_access(manifest)
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            workbench_id: String,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": "Gateway store not available"
            }))?);
        };

        let checkpoints = store.list_checkpoints_for_workbench(&args.workbench_id)?;

        Ok(serde_json::to_string(&serde_json::json!({
            "ok": true,
            "workbench_id": args.workbench_id,
            "checkpoints": checkpoints,
            "count": checkpoints.len(),
        }))?)
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}

pub struct WorkbenchCheckoutTool;

impl NativeTool for WorkbenchCheckoutTool {
    fn name(&self) -> &'static str {
        "workbench_checkout"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Restore a workbench to a previous checkpoint. Current files are replaced with the checkpoint snapshot.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "checkpoint_id": {
                        "type": "string",
                        "description": "The checkpoint ID to restore"
                    }
                },
                "required": ["checkpoint_id"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        has_workbench_access(manifest)
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            checkpoint_id: String,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": "Gateway store not available"
            }))?);
        };

        let Some(cp) = store.load_checkpoint(&args.checkpoint_id)? else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": "Checkpoint not found"
            }))?);
        };

        let Some(wb) = store.load_workbench(&cp.workbench_id)? else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": "Workbench not found"
            }))?);
        };

        if wb.status != WorkbenchStatus::Active {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": format!("Cannot checkout to a {} workbench", wb.status.as_str())
            }))?);
        }

        let source_dir = Path::new(&wb.workspace_path);
        let checkpoint_dir = source_dir
            .parent()
            .unwrap()
            .join(".autonoetic")
            .join("checkpoints")
            .join(&cp.checkpoint_id);

        if !checkpoint_dir.exists() {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": "Checkpoint files not found on disk"
            }))?);
        }

        let current_files = collect_workbench_files(source_dir)?;
        for (name, _) in &current_files {
            let path = source_dir.join(name);
            let _ = std::fs::remove_file(&path);
        }

        let cp_files = collect_workbench_files(&checkpoint_dir)?;
        for (name, _) in &cp_files {
            let src = checkpoint_dir.join(name);
            let dst = source_dir.join(name);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&src, &dst)?;
        }

        Ok(serde_json::to_string(&serde_json::json!({
            "ok": true,
            "workbench_id": wb.workbench_id,
            "restored_from": cp.checkpoint_id,
            "label": cp.label,
            "file_count": cp.file_count,
            "message": "Workbench restored to checkpoint."
        }))?)
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}
