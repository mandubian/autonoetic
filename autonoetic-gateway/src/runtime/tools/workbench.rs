use crate::artifact_store::ArtifactStore;
use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::content_store::ContentStore;
use crate::runtime::semantic_diff::RuleBasedSemanticSummarizer;
use crate::runtime::tools::{NativeTool, NativeToolRegistry, ToolMetadata};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::tool_error::ToolError;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::semantic_diff::{SemanticSummary, SemanticSummaryInputs, SemanticSummarizer};
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
    registry.register(Box::new(WorkbenchReconcileTool));
    registry.register(Box::new(WorkbenchDiscardTool));
    registry.register(Box::new(WorkbenchCleanupTool));
}

fn has_workbench_access(manifest: &AgentManifest) -> bool {
    manifest.capabilities.iter().any(|c| {
        matches!(c, Capability::PlanFrameAccess { .. })
    })
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Emit a `workbench.{created,reconciled,discarded}` event onto the canonical
/// Session Room timeline (#363 P1), attributed to the acting agent's seat and
/// referencing the workbench surface. Best-effort — never fails the operation.
fn emit_workbench_timeline_event(
    store: &crate::scheduler::gateway_store::GatewayStore,
    root_session_id: &str,
    agent_id: &str,
    workbench_id: &str,
    event_type: &str,
    altitude: Option<autonoetic_types::session_timeline::Altitude>,
) {
    let role = crate::runtime::session_timeline::derive_role(agent_id);
    let principal = autonoetic_types::principal::Principal::agent(agent_id);
    let refs = autonoetic_types::session_timeline::TimelineRefs {
        workbench_id: Some(workbench_id.to_string()),
        ..Default::default()
    };
    let event = crate::runtime::session_timeline::build_timeline_event(
        root_session_id.to_string(),
        root_session_id.to_string(),
        None,
        &principal,
        &role,
        event_type,
        altitude,
        Some(serde_json::json!({ "workbench_id": workbench_id })),
        refs,
    );
    if let Err(e) = store.create_live_digest_event(&event) {
        tracing::debug!(target: "session_timeline", error = %e, "workbench timeline emit failed");
    }
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

fn mint_artifact_ref_id() -> String {
    let b = *uuid::Uuid::new_v4().as_bytes();
    format!(
        "ar.{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5]
    )
}

/// Create a checkpoint of the workbench's current files. Used for both
/// manual checkpoints (the `workbench_checkpoint` tool) and automatic
/// checkpoints (on projection and before reconcile). Returns the
/// checkpoint id on success.
///
/// For **automatic** callers the result is discarded (`let _ = …`) so
/// a checkpoint failure is non-fatal.  The **manual** `workbench_checkpoint`
/// tool propagates the error so the operator gets feedback.
fn create_auto_checkpoint(
    store: &crate::scheduler::gateway_store::GatewayStore,
    wb: &WorkbenchProjection,
    label: &str,
) -> Result<String, anyhow::Error> {
    let source_dir = Path::new(&wb.workspace_path);
    if !source_dir.exists() {
        return Err(anyhow::anyhow!("workbench directory gone"));
    }
    let files = collect_workbench_files(source_dir)?;
    if files.is_empty() {
        return Err(anyhow::anyhow!("no files to checkpoint"));
    }
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
        label: Some(label.to_string()),
        file_count: files.len(),
        total_bytes,
        created_at: now.clone(),
    };

    store.save_checkpoint(&cp)?;
    store.update_workbench_last_checkpoint(&wb.workbench_id, &now)?;

    Ok(checkpoint_id)
}

fn compute_diff(
    source_dir: &Path,
    base_digests: &std::collections::HashMap<String, String>,
) -> anyhow::Result<Vec<WorkbenchFileDiff>> {
    let current_files = collect_workbench_files(source_dir)?;
    let current_names: std::collections::HashSet<&str> =
        current_files.iter().map(|(n, _)| n.as_str()).collect();

    let mut diffs: Vec<WorkbenchFileDiff> = Vec::new();

    for name in base_digests.keys() {
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
    Ok(diffs)
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
            return Ok(ToolError::execution("Gateway directory not available", Some("Ensure the gateway data directory is configured and accessible.")).with_code("gateway_dir_unavailable").to_error_response());
        };

        let Some(store) = gateway_store else {
            return Ok(ToolError::execution("Gateway store not available", Some("Ensure the gateway database is initialized and the store path is accessible.")).with_code("gateway_store_unavailable").to_error_response());
        };

        let Some(config) = config else {
            return Ok(ToolError::execution("Gateway config not available", Some("Ensure the gateway configuration is loaded and valid.")).with_code("gateway_config_unavailable").to_error_response());
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
        emit_workbench_timeline_event(
            &store,
            &wb.root_session_id,
            &manifest.agent.id,
            &wb.workbench_id,
            "workbench.created",
            None,
        );

        // Issue #330: auto-checkpoint on projection so the operator has
        // a clean restore point before any edits. Best-effort — failure
        // does not block the projection.
        let _ = create_auto_checkpoint(&store, &wb, "auto: projection");

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
            return Ok(ToolError::execution("Gateway store not available", Some("Ensure the gateway database is initialized and the store path is accessible.")).with_code("gateway_store_unavailable").to_error_response());
        };

        let Some(wb) = store.load_workbench(&args.workbench_id)? else {
            return Ok(ToolError::not_found("Workbench", Some("Create a workbench first or check the workbench ID.")).with_code("workbench_not_found").to_error_response());
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
            return Ok(ToolError::execution("Gateway store not available", Some("Ensure the gateway database is initialized and the store path is accessible.")).with_code("gateway_store_unavailable").to_error_response());
        };

        let Some(wb) = store.load_workbench(&args.workbench_id)? else {
            return Ok(ToolError::not_found("Workbench", Some("Create a workbench first or check the workbench ID.")).with_code("workbench_not_found").to_error_response());
        };

        if wb.status != WorkbenchStatus::Active {
            return Ok(ToolError::conflict(format!("Workbench is in '{}' status", wb.status.as_str()), Some("Ensure the workbench is in Active status before performing this operation.")).with_code("workbench_wrong_status").to_error_response());
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

        let diffs = compute_diff(source_dir, &base_digests)?;
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
            return Ok(ToolError::execution("Gateway store not available", Some("Ensure the gateway database is initialized and the store path is accessible.")).with_code("gateway_store_unavailable").to_error_response());
        };

        let Some(wb) = store.load_workbench(&args.workbench_id)? else {
            return Ok(ToolError::not_found("Workbench", Some("Create a workbench first or check the workbench ID.")).with_code("workbench_not_found").to_error_response());
        };

        if wb.status != WorkbenchStatus::Active {
            return Ok(ToolError::conflict(format!("Cannot checkpoint a {} workbench", wb.status.as_str()), Some("Ensure the workbench is in Active status before checkpointing.")).with_code("workbench_wrong_status").to_error_response());
        }

        let label = args.label.as_deref().unwrap_or("manual");
        let cp_id = match create_auto_checkpoint(&store, &wb, label) {
            Ok(id) => id,
            Err(e) => {
                return Ok(ToolError::execution(format!("Checkpoint failed: {e}"), Some("Check the underlying storage and retry.")).with_code("checkpoint_failed").to_error_response());
            }
        };

        Ok(serde_json::to_string(&serde_json::json!({
            "ok": true,
            "checkpoint_id": cp_id,
            "workbench_id": wb.workbench_id,
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
            return Ok(ToolError::execution("Gateway store not available", Some("Ensure the gateway database is initialized and the store path is accessible.")).with_code("gateway_store_unavailable").to_error_response());
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
            return Ok(ToolError::execution("Gateway store not available", Some("Ensure the gateway database is initialized and the store path is accessible.")).with_code("gateway_store_unavailable").to_error_response());
        };

        let Some(cp) = store.load_checkpoint(&args.checkpoint_id)? else {
            return Ok(ToolError::not_found("Checkpoint", Some("Create a checkpoint first or check the checkpoint ID.")).with_code("checkpoint_not_found").to_error_response());
        };

        let Some(wb) = store.load_workbench(&cp.workbench_id)? else {
            return Ok(ToolError::not_found("Workbench", Some("Create a workbench first or check the workbench ID.")).with_code("workbench_not_found").to_error_response());
        };

        if wb.status != WorkbenchStatus::Active {
            return Ok(ToolError::conflict(format!("Cannot checkout to a {} workbench", wb.status.as_str()), Some("Ensure the workbench is in Active status before checkout.")).with_code("workbench_wrong_status").to_error_response());
        }

        let source_dir = Path::new(&wb.workspace_path);
        let checkpoint_dir = source_dir
            .parent()
            .unwrap()
            .join(".autonoetic")
            .join("checkpoints")
            .join(&cp.checkpoint_id);

        if !checkpoint_dir.exists() {
            return Ok(ToolError::resource("Checkpoint files not found on disk", Some("Ensure the checkpoint directory exists and the files have not been manually removed.")).with_code("checkpoint_files_missing").to_error_response());
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

pub struct WorkbenchReconcileTool;

impl NativeTool for WorkbenchReconcileTool {
    fn name(&self) -> &'static str {
        "workbench_reconcile"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Reconcile workbench edits into a new immutable artifact revision. Reads current files from the workbench, classifies authorship (operator-modified vs agent-generated), builds a new artifact, and marks the workbench as reconciled.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "workbench_id": {
                        "type": "string",
                        "description": "The workbench ID to reconcile"
                    },
                    "message": {
                        "type": "string",
                        "description": "Optional human-readable message describing the edits"
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
            workbench_id: String,
            message: Option<String>,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(gateway_dir) = gateway_dir else {
            return Ok(ToolError::execution("Gateway directory not available", Some("Ensure the gateway data directory is configured and accessible.")).with_code("gateway_dir_unavailable").to_error_response());
        };

        let Some(store) = gateway_store else {
            return Ok(ToolError::execution("Gateway store not available", Some("Ensure the gateway database is initialized and the store path is accessible.")).with_code("gateway_store_unavailable").to_error_response());
        };

        let Some(config) = config else {
            return Ok(ToolError::execution("Gateway config not available", Some("Ensure the gateway configuration is loaded and valid.")).with_code("gateway_config_unavailable").to_error_response());
        };

        let session_id_val = session_id.ok_or_else(|| anyhow::anyhow!("session_id required"))?;
        let root_session_id = session_id_val.split('/').next().unwrap_or(session_id_val);

        let Some(wb) = store.load_workbench(&args.workbench_id)? else {
            return Ok(ToolError::not_found("Workbench", Some("Create a workbench first or check the workbench ID.")).with_code("workbench_not_found").to_error_response());
        };

        if wb.status != WorkbenchStatus::Active {
            return Ok(ToolError::conflict(format!("Cannot reconcile a {} workbench", wb.status.as_str()), Some("Ensure the workbench is in Active status before reconciliation.")).with_code("workbench_wrong_status").to_error_response());
        }

        let source_dir = Path::new(&wb.workspace_path);
        if !source_dir.exists() {
            return Ok(ToolError::resource("Workbench source directory does not exist", Some("Ensure the workbench workspace path exists on disk.")).with_code("workbench_source_missing").to_error_response());
        }

        let meta_dir = source_dir.parent().unwrap().join(".autonoetic");
        let digests_path = meta_dir.join("base_digests.json");
        let base_digests: std::collections::HashMap<String, String> = if digests_path.exists() {
            serde_json::from_str(&std::fs::read_to_string(&digests_path)?)?
        } else {
            std::collections::HashMap::new()
        };

        let diffs = compute_diff(source_dir, &base_digests)?;

        let current_files = collect_workbench_files(source_dir)?;
        if current_files.is_empty() {
            return Ok(ToolError::conflict("No files in workbench to reconcile", Some("Add files to the workbench source directory, then retry.")).with_code("workbench_empty").to_error_response());
        }

        let content_store = ContentStore::new(gateway_dir)?;
        let mut input_names: Vec<String> = Vec::new();

        for (name, _) in &current_files {
            let file_path = source_dir.join(name);
            let content = std::fs::read(&file_path)?;
            let handle = content_store.write(&content)?;
            content_store.register_name(root_session_id, name, &handle)?;
            input_names.push(name.clone());
        }

        let artifact_store = ArtifactStore::new(gateway_dir)?;
        let bundle = artifact_store.build(&input_names, None, None, root_session_id)?;

        let root = crate::runtime::content_store::root_session_id(root_session_id);
        let (scope_type, scope_id) =
            match crate::scheduler::workflow_store::resolve_workflow_id_for_root_session(
                config, root,
            ) {
                Ok(Some(wf_id)) => (
                    autonoetic_types::artifact::ArtifactRefScopeType::Workflow,
                    wf_id,
                ),
                _ => (
                    autonoetic_types::artifact::ArtifactRefScopeType::Session,
                    root.to_string(),
                ),
            };

        let ref_id = mint_artifact_ref_id();
        store.create_artifact_ref(&autonoetic_types::artifact::ArtifactRefRecord {
            ref_id: ref_id.clone(),
            scope_type,
            scope_id: scope_id.clone(),
            artifact_id: bundle.artifact_id.clone(),
            artifact_manifest_digest: bundle.artifact_manifest_digest.clone(),
            artifact_canonical_digest: bundle.artifact_canonical_digest.clone(),
            created_by_agent_id: manifest.agent.id.clone(),
            created_at: now_rfc3339(),
            expires_at: None,
            revoked_at: None,
        })?;

        // Issue #330: auto-checkpoint before reconcile so the operator
        // can restore the pre-reconcile state if needed. Best-effort.
        if let Ok(_cp_id) = create_auto_checkpoint(&store, &wb, "auto: pre-reconcile") {
            // last_checkpoint_at updated inside create_auto_checkpoint
        }

        let now = now_rfc3339();
        store.update_workbench_status(&wb.workbench_id, WorkbenchStatus::Reconciled, &now)?;
        emit_workbench_timeline_event(
            &store,
            &wb.root_session_id,
            &manifest.agent.id,
            &wb.workbench_id,
            "workbench.reconciled",
            None,
        );

        let semantic_summary = build_semantic_summary(
            &store,
            &wb,
            &scope_id,
            &bundle.artifact_id,
            &diffs,
            source_dir,
            &content_store,
            &now,
        )?;

        let propose_waivers = config.validation_waivers.enabled
            && config.validation_waivers.auto_propose_after_reconcile;

        let provenance = serde_json::json!({
            "base_artifact_id": wb.base_artifact_id,
            "new_artifact_id": bundle.artifact_id,
            "new_artifact_ref": ref_id,
            "operator_modified": diffs.iter().filter(|d| d.change_type == FileChangeType::Modified).map(|d| d.path.clone()).collect::<Vec<_>>(),
            "operator_added": diffs.iter().filter(|d| d.change_type == FileChangeType::Added).map(|d| d.path.clone()).collect::<Vec<_>>(),
            "deleted": diffs.iter().filter(|d| d.change_type == FileChangeType::Deleted).map(|d| d.path.clone()).collect::<Vec<_>>(),
            "unchanged": diffs.iter().filter(|d| d.change_type == FileChangeType::Unchanged).count(),
            "propose_waivers": propose_waivers,
        });

        let provenance_path = meta_dir.join("reconciliation.json");
        std::fs::write(&provenance_path, serde_json::to_string_pretty(&provenance)?)?;

        let summary_path = meta_dir.join("semantic_summary.json");
        if let Ok(json) = serde_json::to_string_pretty(&semantic_summary) {
            let _ = std::fs::write(&summary_path, json);
        }

        let changed = diffs.iter().filter(|d| d.change_type != FileChangeType::Unchanged).count();

        Ok(serde_json::to_string(&serde_json::json!({
            "ok": true,
            "workbench_id": wb.workbench_id,
            "new_artifact_ref": ref_id,
            "new_artifact_id": bundle.artifact_id,
            "base_artifact_id": wb.base_artifact_id,
            "total_files": current_files.len(),
            "changed_files": changed,
            "provenance": provenance,
            "semantic_summary": semantic_summary,
            "reconciled_at": now,
            "propose_waivers": propose_waivers,
            "message": args.message.unwrap_or_default(),
        }))?)
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}

/// Build a [`SemanticSummary`] for the workbench's reconciled diff.
///
/// The summarizer is the deterministic rule-based default; callers can
/// swap in a different `SemanticSummarizer` impl later without changing
/// the rest of the reconcile path. Failures here are *not* fatal to the
/// reconcile itself — the raw provenance in `reconciliation.json`
/// remains the source of truth, and a missing `semantic_summary.json`
/// just means the wake-up payload will not carry a summary.
fn build_semantic_summary(
    store: &std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>,
    wb: &WorkbenchProjection,
    scope_id: &str,
    new_artifact_id: &str,
    diffs: &[WorkbenchFileDiff],
    source_dir: &Path,
    content_store: &ContentStore,
    generated_at: &str,
) -> anyhow::Result<SemanticSummary> {
    let summarizer = RuleBasedSemanticSummarizer::default();

    let mut current_files: std::collections::HashMap<String, Vec<u8>> =
        std::collections::HashMap::new();
    let mut base_files: std::collections::HashMap<String, Vec<u8>> =
        std::collections::HashMap::new();
    for d in diffs {
        match d.change_type {
            FileChangeType::Added | FileChangeType::Modified => {
                let path = source_dir.join(&d.path);
                if let Ok(bytes) = std::fs::read(&path) {
                    current_files.insert(d.path.clone(), bytes);
                }
            }
            FileChangeType::Deleted => {
                if let Some(digest) = &d.base_digest {
                    if let Ok(bytes) = content_store.read(digest) {
                        base_files.insert(d.path.clone(), bytes);
                    }
                }
            }
            FileChangeType::Unchanged => {}
        }
    }

    let plan_summary = if wb.workflow_id.is_empty() {
        None
    } else {
        store
            .load_active_plan_for_workflow(&wb.workflow_id)
            .ok()
            .flatten()
            .map(|p| p.compact_summary())
    };
    let _ = scope_id;

    let waivers_by_validation: std::collections::HashMap<String, Vec<String>> =
        match store.list_waivers_for_artifact(&wb.base_artifact_id) {
            Ok(rows) => {
                let mut map: std::collections::HashMap<String, Vec<String>> =
                    std::collections::HashMap::new();
                for w in rows {
                    map.entry(w.validation_id)
                        .or_default()
                        .push(w.waiver_id);
                }
                map
            }
            Err(_) => std::collections::HashMap::new(),
        };

    let inputs = SemanticSummaryInputs {
        workbench_id: &wb.workbench_id,
        base_artifact_id: &wb.base_artifact_id,
        new_artifact_id,
        diffs,
        current_files: &current_files,
        base_files: &base_files,
        plan: plan_summary.as_ref(),
        waivers_by_validation: &waivers_by_validation,
        generated_at,
    };

    Ok(summarizer.summarize(&inputs))
}

pub struct WorkbenchDiscardTool;

impl NativeTool for WorkbenchDiscardTool {
    fn name(&self) -> &'static str {
        "workbench_discard"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Discard a workbench without reconciling. Marks the workbench as discarded. The workbench directory and checkpoints are preserved for audit but the workbench can no longer be edited or reconciled.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "workbench_id": {
                        "type": "string",
                        "description": "The workbench ID to discard"
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
            return Ok(ToolError::execution("Gateway store not available", Some("Ensure the gateway database is initialized and the store path is accessible.")).with_code("gateway_store_unavailable").to_error_response());
        };

        let Some(wb) = store.load_workbench(&args.workbench_id)? else {
            return Ok(ToolError::not_found("Workbench", Some("Create a workbench first or check the workbench ID.")).with_code("workbench_not_found").to_error_response());
        };

        if wb.status != WorkbenchStatus::Active {
            return Ok(ToolError::conflict(format!("Cannot discard a {} workbench", wb.status.as_str()), Some("Ensure the workbench is in Active status before discarding.")).with_code("workbench_wrong_status").to_error_response());
        }

        let now = now_rfc3339();
        store.update_workbench_status(&wb.workbench_id, WorkbenchStatus::Discarded, &now)?;
        emit_workbench_timeline_event(
            &store,
            &wb.root_session_id,
            &_manifest.agent.id,
            &wb.workbench_id,
            "workbench.discarded",
            None,
        );

        Ok(serde_json::to_string(&serde_json::json!({
            "ok": true,
            "workbench_id": wb.workbench_id,
            "status": "discarded",
            "discarded_at": now,
            "message": "Workbench discarded. Files preserved for audit."
        }))?)
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}

pub struct WorkbenchCleanupTool;

impl NativeTool for WorkbenchCleanupTool {
    fn name(&self) -> &'static str {
        "workbench_cleanup"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Clean up a reconciled or discarded workbench. Deletes the workbench directory, all checkpoints, and the SQLite record. Active workbenches are refused — they must be reconciled or discarded first.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "workbench_id": {
                        "type": "string",
                        "description": "The workbench ID to clean up"
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
            return Ok(ToolError::execution("Gateway store not available", Some("Ensure the gateway database is initialized and the store path is accessible.")).with_code("gateway_store_unavailable").to_error_response());
        };

        let Some(wb) = store.load_workbench(&args.workbench_id)? else {
            return Ok(ToolError::not_found("Workbench", Some("Create a workbench first or check the workbench ID.")).with_code("workbench_not_found").to_error_response());
        };

        if wb.status == WorkbenchStatus::Active {
            return Ok(ToolError::conflict("Cannot clean up an active workbench. Reconcile or discard first.", Some("Reconcile or discard the workbench before cleanup.")).with_code("workbench_wrong_status").to_error_response());
        }

        let workspace_path = Path::new(&wb.workspace_path);
        let parent = workspace_path.parent().unwrap();
        let meta_dir = parent.join(".autonoetic");

        let mut warnings: Vec<String> = Vec::new();

        let checkpoints_dir = meta_dir.join("checkpoints");
        if checkpoints_dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&checkpoints_dir) {
                warnings.push(format!("Failed to remove checkpoints dir: {e}"));
            }
        }

        if workspace_path.exists() {
            if let Err(e) = std::fs::remove_dir_all(workspace_path) {
                warnings.push(format!("Failed to remove workspace dir: {e}"));
            }
        }

        if meta_dir.exists() {
            for stem in &["projection", "base_digests", "reconciliation", "semantic_summary"] {
                let p = meta_dir.join(format!("{stem}.json"));
                let _ = std::fs::remove_file(&p);
            }
            if std::fs::read_dir(&meta_dir).map_or(true, |mut d| d.next().is_none()) {
                let _ = std::fs::remove_dir(&meta_dir);
            }
        }

        if parent.exists() {
            if std::fs::read_dir(parent).map_or(true, |mut d| d.next().is_none()) {
                let _ = std::fs::remove_dir(parent);
            }
        }

        store.delete_workbench(&wb.workbench_id)?;

        let mut response = serde_json::json!({
            "ok": true,
            "workbench_id": wb.workbench_id,
            "message": format!("Cleaned up {} workbench.", wb.status.as_str())
        });
        if !warnings.is_empty() {
            response["warnings"] = serde_json::json!(warnings);
        }
        Ok(serde_json::to_string(&response)?)
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}
