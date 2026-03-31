use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{validate_relative_agent_path, NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::runtime_lock::{LockedLayerMount, RuntimeLock};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(AgentRevisionCreateTool));
    registry.register(Box::new(AgentRevisionListTool));
    registry.register(Box::new(AgentRevisionInspectTool));
    registry.register(Box::new(AgentRevisionPromoteTool));
    registry.register(Box::new(AgentRevisionRollbackTool));
    registry.register(Box::new(AgentRevisionDiffTool));
}

fn normalize_runtime_lock(lock: RuntimeLock) -> RuntimeLock {
    let mut normalized = lock;
    normalized
        .dependencies
        .sort_by(|a, b| a.runtime.cmp(&b.runtime));
    for dep in &mut normalized.dependencies {
        dep.packages.sort();
    }
    normalized.artifacts.sort_by(|a, b| {
        (&a.name, &a.version, &a.sha256, &a.source)
            .cmp(&(&b.name, &b.version, &b.sha256, &b.source))
    });
    normalized.layers.sort_by(|a, b| {
        (&a.mount_path, &a.layer_id, &a.digest).cmp(&(&b.mount_path, &b.layer_id, &b.digest))
    });
    normalized
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn canonical_runtime_lock_bytes(lock: RuntimeLock) -> anyhow::Result<Vec<u8>> {
    let normalized = normalize_runtime_lock(lock);
    Ok(serde_json::to_vec(&normalized)?)
}

fn compute_revision_content_digest_hex(files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut hasher = Sha256::new();
    for (path, bytes) in files {
        hasher.update(path.as_bytes());
        hasher.update([0_u8]);
        hasher.update(bytes);
        hasher.update([0_u8]);
    }
    format!("{:x}", hasher.finalize())
}

fn materialize_revision_directory(
    gateway_dir: &Path,
    agent_id: &str,
    revision_id: &str,
    files: &BTreeMap<String, Vec<u8>>,
) -> anyhow::Result<std::path::PathBuf> {
    let revision_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join(agent_id)
        .join(revision_id);

    if revision_dir.exists() {
        return Ok(revision_dir);
    }

    let tmp_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join(agent_id)
        .join(format!(".tmp-{}-{}", revision_id, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp_dir)?;

    for (path, bytes) in files {
        validate_relative_agent_path(path)?;
        let output = tmp_dir.join(path);
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&output, bytes)?;
    }

    if let Some(parent) = revision_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(&tmp_dir, &revision_dir) {
        Ok(()) => Ok(revision_dir),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            if revision_dir.exists() {
                Ok(revision_dir)
            } else {
                Err(e.into())
            }
        }
    }
}

fn collect_revision_files(root: &Path) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    fn walk(
        base: &Path,
        current: &Path,
        out: &mut BTreeMap<String, Vec<u8>>,
    ) -> anyhow::Result<()> {
        for entry in std::fs::read_dir(current)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out)?;
                continue;
            }
            if !path.is_file() {
                continue;
            }
            let rel = path
                .strip_prefix(base)
                .map_err(|e| anyhow::anyhow!("Failed to compute relative path: {}", e))?;
            let rel = rel.to_string_lossy().replace('\\', "/");
            let bytes = std::fs::read(&path)?;
            out.insert(rel, bytes);
        }
        Ok(())
    }

    let mut files = BTreeMap::new();
    walk(root, root, &mut files)?;
    Ok(files)
}

#[derive(Debug, Deserialize)]
struct RevisionCreateArgs {
    agent_id: String,
    artifact_id: String,
    #[serde(default, alias = "base_ref")]
    base_revision_id: Option<String>,
    #[serde(default, alias = "change_summary")]
    summary: Option<String>,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

pub struct AgentRevisionCreateTool;

impl NativeTool for AgentRevisionCreateTool {
    fn name(&self) -> &'static str {
        "agent.revision.create"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::AgentRevision { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Create a new immutable agent revision from an artifact bundle. The revision is stored but not activated until promoted.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string", "description": "Logical agent ID for this revision" },
                    "artifact_id": { "type": "string", "description": "Artifact ID containing the agent bundle (SKILL.md + files)" },
                    "base_revision_id": { "type": "string", "description": "Optional: base revision this is derived from" },
                    "summary": { "type": "string", "description": "Optional: human-readable summary of changes" }
                },
                "required": ["agent_id", "artifact_id"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: RevisionCreateArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments: {}", e))?;

        crate::runtime::tools::validate_agent_id(&args.agent_id)?;
        anyhow::ensure!(
            !args.artifact_id.trim().is_empty(),
            "artifact_id must not be empty"
        );
        anyhow::ensure!(
            policy.can_agent_revision(&args.agent_id),
            "Permission Denied: agent '{}' lacks AgentRevision capability for '{}'",
            manifest.agent.id,
            args.agent_id
        );

        let Some(gateway_store) = gateway_store else {
            return Err(anyhow::anyhow!(
                "GatewayStore is required for revision creation"
            ));
        };

        let gateway_dir = gateway_dir.ok_or_else(|| anyhow::anyhow!("gateway_dir required"))?;
        let artifact = crate::ArtifactStore::new(gateway_dir)?;

        let bundle = artifact
            .inspect(&args.artifact_id)
            .map_err(|e| anyhow::anyhow!("Artifact '{}' not found: {}", args.artifact_id, e))?;
        anyhow::ensure!(
            bundle.kind == autonoetic_types::artifact::ArtifactKind::AgentBundle,
            "Artifact '{}' has kind '{:?}'. agent.revision.create requires kind 'agent_bundle'.",
            args.artifact_id,
            bundle.kind
        );

        let files = artifact.resolve_files(&args.artifact_id)?;
        let mut file_map: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for (path, bytes) in files {
            validate_relative_agent_path(&path)?;
            anyhow::ensure!(
                file_map.insert(path.clone(), bytes).is_none(),
                "Artifact contains duplicate file path '{}'",
                path
            );
        }

        let skill_content = file_map
            .get("SKILL.md")
            .ok_or_else(|| anyhow::anyhow!("Agent bundle artifact must include SKILL.md"))?
            .clone();
        let skill_text = String::from_utf8_lossy(&skill_content);
        let (bundle_manifest, _instructions) =
            crate::runtime::parser::SkillParser::parse(&skill_text)
                .map_err(|e| anyhow::anyhow!("Failed to parse SKILL.md from artifact: {}", e))?;
        anyhow::ensure!(
            bundle_manifest.agent.id == args.agent_id,
            "Bundle SKILL.md declares agent.id '{}' but revision was requested for '{}'. \
             The artifact must match the requested agent identity.",
            bundle_manifest.agent.id,
            args.agent_id
        );

        let lock_rel_path = bundle_manifest.runtime.runtime_lock.clone();
        validate_relative_agent_path(&lock_rel_path)?;
        let lock_content = file_map.get(&lock_rel_path).ok_or_else(|| {
            anyhow::anyhow!(
                "Agent bundle artifact must include '{}' declared in SKILL.md runtime.runtime_lock",
                lock_rel_path
            )
        })?;
        let parsed_lock: RuntimeLock = serde_yaml::from_slice(lock_content).map_err(|e| {
            anyhow::anyhow!(
                "Failed to parse '{}' from artifact '{}': {}",
                lock_rel_path,
                args.artifact_id,
                e
            )
        })?;

        let expected_layers: Vec<LockedLayerMount> = {
            let mut layers: Vec<LockedLayerMount> = bundle
                .layers
                .iter()
                .map(|layer| LockedLayerMount {
                    layer_id: layer.layer_id.clone(),
                    digest: layer.digest.clone(),
                    mount_path: layer.mount_path.clone(),
                })
                .collect();
            layers.sort_by(|a, b| {
                (&a.mount_path, &a.layer_id, &a.digest).cmp(&(
                    &b.mount_path,
                    &b.layer_id,
                    &b.digest,
                ))
            });
            layers
        };

        let normalized_lock = normalize_runtime_lock(parsed_lock);
        anyhow::ensure!(
            normalized_lock.layers == expected_layers,
            "runtime.lock layer closure does not match artifact layers: runtime.lock has {} layer(s), artifact has {} layer(s)",
            normalized_lock.layers.len(),
            expected_layers.len()
        );

        let canonical_lock_bytes = canonical_runtime_lock_bytes(normalized_lock.clone())?;
        file_map.insert(lock_rel_path, canonical_lock_bytes.clone());

        let manifest_hash = format!("sha256:{}", sha256_hex(&skill_content));
        let runtime_lock_hash = format!("sha256:{}", sha256_hex(&canonical_lock_bytes));
        let revision_digest_hex = compute_revision_content_digest_hex(&file_map);
        let revision_id = format!("rev_sha256:{}", revision_digest_hex);
        let content_digest = format!("sha256:{}", revision_digest_hex);

        if let Some(existing_rev) = gateway_store.get_agent_revision(&revision_id)? {
            let _ = materialize_revision_directory(
                gateway_dir,
                &args.agent_id,
                &revision_id,
                &file_map,
            )?;
            return Ok(serde_json::json!({
                "ok": true,
                "status": "already_exists",
                "revision_id": revision_id,
                "agent_id": args.agent_id,
                "agent_ref": format!("{}@{}", args.agent_id, revision_id),
                "short_ref": format!("{}@rev_{}", args.agent_id, existing_rev.short_id),
            })
            .to_string());
        }

        let _revision_dir =
            materialize_revision_directory(gateway_dir, &args.agent_id, &revision_id, &file_map)?;

        let now = chrono::Utc::now().to_rfc3339();

        let base_revision_id = args.base_revision_id.as_ref().map(|value| {
            if let Some(parsed) = autonoetic_types::agent_revision::AgentRef::parse(value) {
                parsed.revision_id
            } else {
                value.to_string()
            }
        });

        let rev = autonoetic_types::agent_revision::AgentRevisionRecord {
            revision_id: revision_id.clone(),
            agent_id: args.agent_id.clone(),
            base_revision_id,
            artifact_id: Some(args.artifact_id.clone()),
            content_digest,
            runtime_lock_hash,
            manifest_hash,
            created_at: now,
            created_by_type: "agent".to_string(),
            created_by_id: manifest.agent.id.clone(),
            source_kind: "artifact".to_string(),
            source_ref: Some(args.artifact_id.clone()),
            origin_node_id: "gateway".to_string(),
            trust_domain: "local".to_string(),
            status: autonoetic_types::agent_revision::AgentRevisionStatus::Candidate,
            metadata_json: serde_json::json!({
                "summary": args.summary,
                "metadata": args.metadata,
            }),
            short_id: String::new(),
        };

        let short_id = gateway_store.insert_agent_revision_transactional(&rev)?;

        let short_ref = format!("{}@rev_{}", args.agent_id, short_id);
        Ok(serde_json::json!({
            "ok": true,
            "status": "created",
            "revision_id": revision_id,
            "agent_ref": format!("{}@{}", args.agent_id, revision_id),
            "short_ref": short_ref,
            "agent_id": args.agent_id,
            "artifact_id": args.artifact_id,
            "next_step": "Use agent.revision.promote to activate this revision"
        })
        .to_string())
    }
}

#[derive(Debug, Deserialize)]
struct RevisionListArgs {
    agent_id: Option<String>,
}

pub struct AgentRevisionListTool;

impl NativeTool for AgentRevisionListTool {
    fn name(&self) -> &'static str {
        "agent.revision.list"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::AgentRevision { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "List agent revisions. Optionally filter by agent_id.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string", "description": "Optional: filter by agent ID" }
                },
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: RevisionListArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments: {}", e))?;

        let Some(gateway_store) = gateway_store else {
            return Err(anyhow::anyhow!("GatewayStore is required"));
        };

        if let Some(agent_id) = &args.agent_id {
            crate::runtime::tools::validate_agent_id(agent_id)?;
            anyhow::ensure!(
                policy.can_agent_revision(agent_id),
                "Permission Denied: missing AgentRevision capability for '{}'",
                agent_id
            );
        }

        let revisions = if let Some(agent_id) = &args.agent_id {
            gateway_store.list_agent_revisions(agent_id)?
        } else {
            gateway_store.list_all_agent_revisions()?
        };

        let items: Vec<serde_json::Value> = revisions
            .into_iter()
            .map(|r| {
                let short_ref = format!("{}@rev_{}", r.agent_id, r.short_id);
                serde_json::json!({
                    "revision_id": r.revision_id,
                    "short_ref": short_ref,
                    "agent_id": r.agent_id,
                    "status": format!("{:?}", r.status),
                    "created_at": r.created_at,
                    "artifact_id": r.artifact_id,
                    "base_revision_id": r.base_revision_id,
                })
            })
            .collect();

        Ok(serde_json::json!({
            "ok": true,
            "revisions": items,
            "count": items.len(),
        })
        .to_string())
    }
}

#[derive(Debug, Deserialize)]
struct RevisionInspectArgs {
    #[serde(default)]
    agent_ref: Option<String>,
    #[serde(default)]
    revision_id: Option<String>,
}

pub struct AgentRevisionInspectTool;

impl NativeTool for AgentRevisionInspectTool {
    fn name(&self) -> &'static str {
        "agent.revision.inspect"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::AgentRevision { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Inspect a specific agent revision's metadata and execution closure."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_ref": { "type": "string", "description": "Agent ref or alias target to inspect" },
                    "revision_id": { "type": "string", "description": "Full revision ID (rev_sha256:...)" }
                },
                "anyOf": [
                    {"required": ["agent_ref"]},
                    {"required": ["revision_id"]}
                ],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: RevisionInspectArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments: {}", e))?;

        let Some(gateway_store) = gateway_store else {
            return Err(anyhow::anyhow!("GatewayStore is required"));
        };

        let revision_id = if let Some(agent_ref_target) = args.agent_ref.as_deref() {
            crate::runtime::tools::resolve_target_to_agent_ref(
                agent_ref_target,
                gateway_store.as_ref(),
            )?
            .revision_id
        } else {
            args.revision_id.clone().ok_or_else(|| {
                anyhow::anyhow!("Either 'agent_ref' or 'revision_id' must be provided")
            })?
        };

        let rev = gateway_store
            .get_agent_revision(&revision_id)?
            .ok_or_else(|| anyhow::anyhow!("Revision '{}' not found", revision_id))?;
        anyhow::ensure!(
            policy.can_agent_revision(&rev.agent_id),
            "Permission Denied: missing AgentRevision capability for '{}'",
            rev.agent_id
        );

        let short_ref = format!("{}@rev_{}", rev.agent_id, rev.short_id);
        Ok(serde_json::json!({
            "ok": true,
            "revision": {
                "revision_id": rev.revision_id,
                "short_ref": short_ref,
                "agent_id": rev.agent_id,
                "status": format!("{:?}", rev.status),
                "created_at": rev.created_at,
                "created_by_type": rev.created_by_type,
                "created_by_id": rev.created_by_id,
                "artifact_id": rev.artifact_id,
                "base_revision_id": rev.base_revision_id,
                "content_digest": rev.content_digest,
                "runtime_lock_hash": rev.runtime_lock_hash,
                "manifest_hash": rev.manifest_hash,
                "source_kind": rev.source_kind,
                "source_ref": rev.source_ref,
                "origin_node_id": rev.origin_node_id,
                "trust_domain": rev.trust_domain,
                "metadata": rev.metadata_json,
            }
        })
        .to_string())
    }
}

#[derive(Debug, Deserialize)]
struct RevisionPromoteArgs {
    agent_id: String,
    revision_id: String,
    reason: Option<String>,
    required_eval_run_id: Option<String>,
}

pub struct AgentRevisionPromoteTool;

impl NativeTool for AgentRevisionPromoteTool {
    fn name(&self) -> &'static str {
        "agent.revision.promote"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::AgentRevision { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Promote a candidate revision to become the active alias target. New sessions will resolve to this revision.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string", "description": "Logical agent ID whose alias should be updated" },
                    "revision_id": { "type": "string", "description": "Revision ID to promote (must be in candidate or ready status)" },
                    "reason": { "type": "string", "description": "Optional: human-readable reason for promotion" },
                    "required_eval_run_id": { "type": "string", "description": "Optional: if provided, promotion requires this eval run to have passed for the target revision" }
                },
                "required": ["agent_id", "revision_id"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: RevisionPromoteArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments: {}", e))?;

        crate::runtime::tools::validate_agent_id(&args.agent_id)?;
        anyhow::ensure!(
            policy.can_agent_revision(&args.agent_id),
            "Permission Denied: missing AgentRevision capability for '{}'",
            args.agent_id
        );

        let Some(gateway_store) = gateway_store else {
            return Err(anyhow::anyhow!("GatewayStore is required"));
        };

        let rev = gateway_store
            .get_agent_revision(&args.revision_id)?
            .ok_or_else(|| anyhow::anyhow!("Revision '{}' not found", args.revision_id))?;

        anyhow::ensure!(
            rev.agent_id == args.agent_id,
            "Revision '{}' belongs to agent '{}', not '{}'",
            args.revision_id,
            rev.agent_id,
            args.agent_id
        );

        anyhow::ensure!(
            matches!(
                rev.status,
                autonoetic_types::agent_revision::AgentRevisionStatus::Candidate
                    | autonoetic_types::agent_revision::AgentRevisionStatus::Ready
            ),
            "Revision '{}' is in status '{:?}', must be Candidate or Ready for promotion",
            args.revision_id,
            rev.status
        );

        if let Some(eval_run_id) = &args.required_eval_run_id {
            let eval_run = gateway_store.get_eval_run(eval_run_id)?;
            anyhow::ensure!(eval_run.is_some(), "Eval run '{}' not found", eval_run_id);
            let eval_run = eval_run.unwrap();
            anyhow::ensure!(
                matches!(
                    eval_run.status,
                    autonoetic_types::evaluation::EvalRunStatus::Passed
                ),
                "Eval run '{}' did not pass (status: {:?})",
                eval_run_id,
                eval_run.status
            );
            anyhow::ensure!(
                eval_run.subject_revision_id == args.revision_id,
                "Eval run '{}' was for revision '{}', not '{}'",
                eval_run_id,
                eval_run.subject_revision_id,
                args.revision_id
            );
        }

        let promotion_id = autonoetic_types::id_format::mint_hashed_prefixed_id(
            "prom-",
            &format!(
                "{}-{}-{}",
                args.agent_id,
                args.revision_id,
                chrono::Utc::now().to_rfc3339()
            ),
        );

        let previous_revision_id = gateway_store.atomic_promote(
            &args.agent_id,
            &args.revision_id,
            &promotion_id,
            "agent",
            &manifest.agent.id,
            args.reason.as_deref(),
            args.required_eval_run_id.as_deref(),
        )?;

        let short_ref = format!("{}@rev_{}", args.agent_id, rev.short_id);
        Ok(serde_json::json!({
            "ok": true,
            "status": "promoted",
            "agent_id": args.agent_id,
            "revision_id": args.revision_id,
            "short_ref": short_ref,
            "previous_revision_id": previous_revision_id,
            "promotion_id": promotion_id,
        })
        .to_string())
    }
}

#[derive(Debug, Deserialize)]
struct RevisionRollbackArgs {
    agent_id: String,
    revision_id: Option<String>,
    reason: Option<String>,
}

pub struct AgentRevisionRollbackTool;

impl NativeTool for AgentRevisionRollbackTool {
    fn name(&self) -> &'static str {
        "agent.revision.rollback"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::AgentRevision { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Rollback an agent alias to a previous revision. If no revision_id is provided, rolls back to the immediately previous revision from promotion history.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string", "description": "Logical agent ID whose alias should be rolled back" },
                    "revision_id": { "type": "string", "description": "Optional: specific revision ID to roll back to (defaults to immediately previous)" },
                    "reason": { "type": "string", "description": "Optional: human-readable reason for rollback" }
                },
                "required": ["agent_id"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: RevisionRollbackArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments: {}", e))?;

        crate::runtime::tools::validate_agent_id(&args.agent_id)?;
        anyhow::ensure!(
            policy.can_agent_revision(&args.agent_id),
            "Permission Denied: missing AgentRevision capability for '{}'",
            args.agent_id
        );

        let Some(gateway_store) = gateway_store else {
            return Err(anyhow::anyhow!("GatewayStore is required"));
        };

        let target_revision_id = if let Some(ref rev_id) = args.revision_id {
            let rev = gateway_store.get_agent_revision(rev_id)?;
            anyhow::ensure!(rev.is_some(), "Revision '{}' not found", rev_id);
            let rev = rev.unwrap();
            anyhow::ensure!(
                rev.agent_id == args.agent_id,
                "Revision '{}' belongs to '{}', not '{}'",
                rev_id,
                rev.agent_id,
                args.agent_id
            );

            let history = gateway_store.list_promotion_history(&args.agent_id)?;
            let in_lineage = history.iter().any(|p| {
                p.new_revision_id == *rev_id
                    || p.previous_revision_id
                        .as_ref()
                        .map_or(false, |r| r == rev_id)
            });
            anyhow::ensure!(
                in_lineage,
                "Revision '{}' is not in the promotion lineage for agent '{}'. \
                 Rollback can only target revisions that were previously active for this agent.",
                rev_id,
                args.agent_id
            );

            rev_id.clone()
        } else {
            let history = gateway_store.list_promotion_history(&args.agent_id)?;
            let prev = history
                .into_iter()
                .next()
                .and_then(|p| p.previous_revision_id);
            anyhow::ensure!(
                prev.is_some(),
                "No previous revision found for agent '{}'. Provide an explicit revision_id.",
                args.agent_id
            );
            prev.unwrap()
        };

        let rev = gateway_store
            .get_agent_revision(&target_revision_id)?
            .ok_or_else(|| anyhow::anyhow!("Revision '{}' not found", target_revision_id))?;

        let promotion_id = autonoetic_types::id_format::mint_hashed_prefixed_id(
            "prom-",
            &format!(
                "{}-{}-{}",
                args.agent_id,
                target_revision_id,
                chrono::Utc::now().to_rfc3339()
            ),
        );

        let previous_revision_id = gateway_store.atomic_rollback(
            &args.agent_id,
            &target_revision_id,
            &promotion_id,
            "agent",
            &manifest.agent.id,
            args.reason.as_deref(),
        )?;

        let short_ref = format!("{}@rev_{}", args.agent_id, rev.short_id);
        Ok(serde_json::json!({
            "ok": true,
            "status": "rolled_back",
            "agent_id": args.agent_id,
            "revision_id": target_revision_id,
            "short_ref": short_ref,
            "previous_revision_id": previous_revision_id,
            "promotion_id": promotion_id,
        })
        .to_string())
    }
}

#[derive(Debug, Deserialize)]
struct RevisionDiffArgs {
    from_ref: String,
    to_ref: String,
}

pub struct AgentRevisionDiffTool;

impl NativeTool for AgentRevisionDiffTool {
    fn name(&self) -> &'static str {
        "agent.revision.diff"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::AgentRevision { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description:
                "Show a deterministic file-level diff between two immutable agent revisions."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "from_ref": { "type": "string", "description": "Baseline target (alias or agent_ref)" },
                    "to_ref": { "type": "string", "description": "Candidate target (alias or agent_ref)" }
                },
                "required": ["from_ref", "to_ref"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: RevisionDiffArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments: {}", e))?;
        anyhow::ensure!(
            !args.from_ref.trim().is_empty(),
            "from_ref must not be empty"
        );
        anyhow::ensure!(!args.to_ref.trim().is_empty(), "to_ref must not be empty");

        let Some(gateway_store) = gateway_store else {
            return Err(anyhow::anyhow!("GatewayStore is required"));
        };
        let gateway_dir = gateway_dir.ok_or_else(|| anyhow::anyhow!("gateway_dir required"))?;

        let from_ref = crate::runtime::tools::resolve_target_to_agent_ref(
            &args.from_ref,
            gateway_store.as_ref(),
        )?;
        let to_ref = crate::runtime::tools::resolve_target_to_agent_ref(
            &args.to_ref,
            gateway_store.as_ref(),
        )?;
        anyhow::ensure!(
            policy.can_agent_revision(&from_ref.agent_id)
                && policy.can_agent_revision(&to_ref.agent_id),
            "Permission Denied: agent '{}' lacks AgentRevision capability for requested targets",
            manifest.agent.id
        );

        let from_dir = gateway_dir
            .join("revisions")
            .join("agents")
            .join(&from_ref.agent_id)
            .join(&from_ref.revision_id);
        let to_dir = gateway_dir
            .join("revisions")
            .join("agents")
            .join(&to_ref.agent_id)
            .join(&to_ref.revision_id);
        anyhow::ensure!(
            from_dir.exists(),
            "Revision directory not found for '{}'",
            from_ref.to_string()
        );
        anyhow::ensure!(
            to_dir.exists(),
            "Revision directory not found for '{}'",
            to_ref.to_string()
        );

        let from_files = collect_revision_files(&from_dir)?;
        let to_files = collect_revision_files(&to_dir)?;

        let mut paths = BTreeSet::new();
        paths.extend(from_files.keys().cloned());
        paths.extend(to_files.keys().cloned());

        let mut added: Vec<String> = Vec::new();
        let mut removed: Vec<String> = Vec::new();
        let mut modified: Vec<serde_json::Value> = Vec::new();

        for path in paths {
            match (from_files.get(&path), to_files.get(&path)) {
                (None, Some(_)) => added.push(path),
                (Some(_), None) => removed.push(path),
                (Some(from), Some(to)) => {
                    if from != to {
                        modified.push(serde_json::json!({
                            "path": path,
                            "from_sha256": format!("sha256:{}", sha256_hex(from)),
                            "to_sha256": format!("sha256:{}", sha256_hex(to)),
                            "from_size": from.len(),
                            "to_size": to.len(),
                        }));
                    }
                }
                (None, None) => {}
            }
        }

        let from_meta = gateway_store.get_agent_revision(&from_ref.revision_id)?;
        let to_meta = gateway_store.get_agent_revision(&to_ref.revision_id)?;

        Ok(serde_json::json!({
            "ok": true,
            "from_ref": from_ref.to_string(),
            "to_ref": to_ref.to_string(),
            "from_runtime_lock_hash": from_meta.as_ref().map(|r| r.runtime_lock_hash.clone()),
            "to_runtime_lock_hash": to_meta.as_ref().map(|r| r.runtime_lock_hash.clone()),
            "from_manifest_hash": from_meta.as_ref().map(|r| r.manifest_hash.clone()),
            "to_manifest_hash": to_meta.as_ref().map(|r| r.manifest_hash.clone()),
            "changed": !added.is_empty() || !removed.is_empty() || !modified.is_empty(),
            "summary": {
                "added": added.len(),
                "removed": removed.len(),
                "modified": modified.len(),
            },
            "added": added,
            "removed": removed,
            "modified": modified,
        })
        .to_string())
    }
}
