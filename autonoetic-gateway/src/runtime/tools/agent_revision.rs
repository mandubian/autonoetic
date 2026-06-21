use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::remote_access::{extract_host_from_url_literal, RemoteAccessAnalyzer};
use crate::runtime::tools::{validate_relative_agent_path, NativeTool, NativeToolRegistry};
use autonoetic_types::agent::{
    AgentIO, AgentIdentity, AgentManifest, ExecutionMode, LlmConfig, Middleware, ScriptInputMode,
};
use autonoetic_types::artifact::{ArtifactBundle, ArtifactKind};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::{CapabilityDeltaGateMode, GatewayConfig};
use autonoetic_types::principal::PrincipalKind;
use autonoetic_types::runtime_lock::{
    LockedArtifact, LockedDependencySet, LockedLayerMount, RuntimeLock,
};
use autonoetic_types::tool_error::tagged;
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(AgentRevisionCreateTool));
    registry.register(Box::new(AgentRevisionCreateFromIntentTool));
    registry.register(Box::new(AgentRevisionSchemaTool));
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
    normalized.credentials.sort_by(|a, b| a.service.cmp(&b.service));
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

fn normalize_script_entry(entry: &str) -> String {
    let first_word = entry.split_whitespace().next().unwrap_or(entry);
    if first_word == "python3"
        || first_word == "python"
        || first_word == "python2"
        || first_word == "node"
        || first_word == "bash"
        || first_word == "sh"
        || first_word == "perl"
        || first_word == "ruby"
    {
        entry
            .split_whitespace()
            .nth(1)
            .unwrap_or(first_word)
            .to_string()
    } else {
        entry.to_string()
    }
}

fn materialize_revision_directory(
    gateway_dir: &Path,
    agent_id: &str,
    revision_id: &str,
    files: &BTreeMap<String, Vec<u8>>,
    script_entry: Option<&str>,
) -> anyhow::Result<std::path::PathBuf> {
    let revision_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join(agent_id)
        .join(revision_id);

    if revision_dir.exists() {
        if let Some(entry) = script_entry {
            let existing = revision_dir.join(entry);
            if existing.is_file() {
                let mut perms = std::fs::metadata(&existing)?.permissions();
                perms.set_mode(perms.mode() | 0o111);
                std::fs::set_permissions(&existing, perms)?;
            }
        }
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

    if let Some(entry) = script_entry {
        let entry_path = tmp_dir.join(entry);
        if entry_path.is_file() {
            let mut perms = std::fs::metadata(&entry_path)?.permissions();
            perms.set_mode(perms.mode() | 0o111);
            std::fs::set_permissions(&entry_path, perms)?;
        }
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

/// LLMs often emit invalid shapes (bare strings, `scopes` instead of `hosts` on NetworkAccess).
/// Try strict [`Capability`] first, then apply a small set of normalizations, then error.
pub fn normalize_capability_from_llm(v: serde_json::Value) -> anyhow::Result<Capability> {
    if let Ok(c) = serde_json::from_value::<Capability>(v.clone()) {
        return Ok(c);
    }
    if let Some(s) = v.as_str() {
        return capability_from_shorthand(s);
    }
    if let Some(obj) = v.as_object() {
        let mut map = obj.clone();
        if let Some(typ) = map.get("type").and_then(|t| t.as_str()) {
            match typ {
                "NetworkAccess" => {
                    if map.contains_key("scopes") {
                        if !map.contains_key("hosts") {
                            if let Some(sc) = map.remove("scopes") {
                                map.insert("hosts".to_string(), sc);
                            }
                        } else {
                            map.remove("scopes");
                        }
                    }
                }
                "CredentialAccess" => {
                    if map.contains_key("scopes") {
                        if !map.contains_key("services") {
                            if let Some(sc) = map.remove("scopes") {
                                map.insert("services".to_string(), sc);
                            }
                        } else {
                            map.remove("scopes");
                        }
                    }
                }
                "ReadAccess" | "WriteAccess" => {
                    if map.contains_key("hosts") && !map.contains_key("scopes") {
                        if let Some(h) = map.remove("hosts") {
                            map.insert("scopes".to_string(), h);
                        }
                    }
                }
                _ => {}
            }
        }
        if let Ok(c) = serde_json::from_value::<Capability>(serde_json::Value::Object(map)) {
            return Ok(c);
        }
    }
    serde_json::from_value::<Capability>(v).map_err(|e| anyhow::anyhow!("{}", e))
}

fn capability_from_shorthand(s: &str) -> anyhow::Result<Capability> {
    match s.trim() {
        "SandboxFunctions" => Err(anyhow::anyhow!(
            "capability 'SandboxFunctions' cannot be a bare string — explicit tool scoping required. \
             Use {{ \"type\": \"SandboxFunctions\", \"allowed\": [\"content.\", \"knowledge.\"] }} instead."
        )),
        "ReadAccess" => Err(anyhow::anyhow!(
            "capability 'ReadAccess' cannot be a bare string — explicit scope required. \
             Use {{ \"type\": \"ReadAccess\", \"scopes\": [\"self.*\"] }} instead."
        )),
        "WriteAccess" => Err(anyhow::anyhow!(
            "capability 'WriteAccess' cannot be a bare string — explicit scope required. \
             Use {{ \"type\": \"WriteAccess\", \"scopes\": [\"self.*\"] }} instead."
        )),
        "NetworkAccess" => Err(anyhow::anyhow!(
            "capability 'NetworkAccess' cannot be a bare string — explicit host scoping required. \
             Use {{ \"type\": \"NetworkAccess\", \"hosts\": [\"api.example.com\"] }} instead."
        )),
        "CodeExecution" => Err(anyhow::anyhow!(
            "capability 'CodeExecution' cannot be a bare string — explicit command patterns required. \
             Use {{ \"type\": \"CodeExecution\", \"patterns\": [\"python*\"] }} instead."
        )),
        "AgentMessage" => Err(anyhow::anyhow!(
            "capability 'AgentMessage' cannot be a bare string — explicit patterns required. \
             Use {{ \"type\": \"AgentMessage\", \"patterns\": [\"*\"] }} instead."
        )),
        "AgentRevision" => Err(anyhow::anyhow!(
            "capability 'AgentRevision' cannot be a bare string — explicit patterns required. \
             Use {{ \"type\": \"AgentRevision\", \"patterns\": [\"*\"] }} instead."
        )),
        "Evaluation" => Err(anyhow::anyhow!(
            "capability 'Evaluation' cannot be a bare string — explicit patterns required. \
             Use {{ \"type\": \"Evaluation\", \"patterns\": [\"*\"] }} instead."
        )),
        "ApprovalQueue" => Err(anyhow::anyhow!(
            "capability 'ApprovalQueue' cannot be a bare string — explicit patterns required. \
             Use {{ \"type\": \"ApprovalQueue\", \"patterns\": [\"*\"] }} instead."
        )),
        "SchedulerSignal" => Err(anyhow::anyhow!(
            "capability 'SchedulerSignal' cannot be a bare string — explicit patterns required. \
             Use {{ \"type\": \"SchedulerSignal\", \"patterns\": [\"*\"] }} instead."
        )),
        "SchedulerAccess" => Err(anyhow::anyhow!(
            "capability 'SchedulerAccess' cannot be a bare string — explicit patterns required. \
             Use {{ \"type\": \"SchedulerAccess\", \"patterns\": [\"scheduler.cron.*\"] }} instead."
        )),
        "CredentialAccess" => Err(anyhow::anyhow!(
            "capability 'CredentialAccess' cannot be a bare string — explicit service scoping required. \
             Use {{ \"type\": \"CredentialAccess\", \"services\": [\"github\"] }} instead."
        )),
        "UserProfileAccess" => Err(anyhow::anyhow!(
            "capability 'UserProfileAccess' cannot be a bare string — explicit scope required. \
             Use {{ \"type\": \"UserProfileAccess\", \"scopes\": [\"basic\"] }} instead."
        )),
        "SkillInstall" => Err(anyhow::anyhow!(
            "capability 'SkillInstall' cannot be a bare string — explicit source scoping required. \
             Use {{ \"type\": \"SkillInstall\", \"allowed_sources\": [\"agentskills.io\"] }} instead."
        )),
        "ConstitutionalProposal" => Err(anyhow::anyhow!(
            "capability 'ConstitutionalProposal' cannot be a bare string — explicit patterns required. \
             Use {{ \"type\": \"ConstitutionalProposal\", \"patterns\": [\"*\"] }} instead."
        )),
        "EmergencyStop" => Err(anyhow::anyhow!(
            "capability 'EmergencyStop' cannot be a bare string — use a tagged object instead. \
             Use {{ \"type\": \"EmergencyStop\" }} instead."
        )),
        "AgentSpawn" | "BackgroundReevaluation" => Err(anyhow::anyhow!(
            "capability '{s}' cannot be a bare string; use a JSON object with required fields (see Capability schema)"
        )),
        other => Err(anyhow::anyhow!(
            "unknown capability '{other}'; use a tagged Capability object, e.g. {{ \"type\": \"ReadAccess\", \"scopes\": [\"*\"] }}"
        )),
    }
}

fn deserialize_capabilities_lenient<'de, D>(deserializer: D) -> Result<Vec<Capability>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let items = Vec::<serde_json::Value>::deserialize(deserializer)?;
    items
        .into_iter()
        .enumerate()
        .map(|(i, v)| {
            normalize_capability_from_llm(v)
                .map_err(|e| serde::de::Error::custom(format!("capabilities[{i}]: {e}")))
        })
        .collect()
}

fn parse_frontmatter_capabilities(
    frontmatter: &serde_yaml::Value,
) -> anyhow::Result<Vec<Capability>> {
    let frontmatter_json = serde_json::to_value(frontmatter).map_err(|e| {
        anyhow::anyhow!(
            "Promotion gate: failed to convert SKILL.md frontmatter to JSON for capability parsing: {e}"
        )
    })?;
    // Canonical SKILL.md (composed via `render_skill_document`) nests
    // capabilities under `metadata.autonoetic.capabilities`. Some hand-
    // crafted SKILL.md files use a top-level `capabilities` field. Accept
    // both, with the canonical location preferred.
    let caps_json = frontmatter_json
        .get("metadata")
        .and_then(|m| m.get("autonoetic"))
        .and_then(|a| a.get("capabilities"))
        .and_then(|v| v.as_array())
        .or_else(|| frontmatter_json.get("capabilities").and_then(|v| v.as_array()))
        .cloned()
        .unwrap_or_default();

    let mut caps = Vec::new();
    let mut parse_errors: Vec<String> = Vec::new();
    for v in caps_json {
        match normalize_capability_from_llm(v) {
            Ok(cap) => caps.push(cap),
            Err(e) => parse_errors.push(e.to_string()),
        }
    }
    if !parse_errors.is_empty() {
        return Err(tagged::Tagged::validation(anyhow::anyhow!(
            "Promotion gate: cannot parse one or more capability entries in SKILL.md: {}",
            parse_errors.join("; ")
        ))
        .into());
    }
    Ok(caps)
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

#[derive(Debug, Deserialize)]
struct RevisionCreateFromIntentArgs {
    agent_id: String,
    #[serde(default)]
    artifact_id: Option<String>,
    #[serde(default)]
    artifact_ref: Option<String>,
    instructions: String,
    description: String,
    #[serde(deserialize_with = "deserialize_capabilities_lenient")]
    capabilities: Vec<Capability>,
    #[serde(default)]
    execution_mode: Option<ExecutionMode>,
    #[serde(default)]
    script_entry: Option<String>,
    #[serde(default)]
    script_input_mode: Option<ScriptInputMode>,
    #[serde(default)]
    llm_preset: Option<String>,
    #[serde(default)]
    llm_overrides: Option<autonoetic_types::agent::LlmOverrides>,
    llm_config: Option<LlmConfig>,
    #[serde(default)]
    io: Option<AgentIO>,
    #[serde(default)]
    middleware: Option<Middleware>,
    #[serde(default, alias = "base_ref")]
    base_revision_id: Option<String>,
    #[serde(default, alias = "change_summary")]
    summary: Option<String>,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
    /// When true, automatically archives any existing active revision before
    /// creating the new one. Required when updating an already-installed agent.
    #[serde(default)]
    replace: bool,
    /// Service names whose credentials should be injected at spawn time.
    /// The gateway writes these into runtime.lock.credentials and resolves
    /// them from the vault when the agent is spawned. Only meaningful for
    /// script-mode agents that read credentials from env vars.
    #[serde(default)]
    credential_services: Vec<String>,
    /// When true, allows `NetworkAccess.hosts: ["*"]` for genuine open-web agents.
    #[serde(default)]
    open_web: bool,
}

fn normalized_llm_preset(preset: &Option<String>) -> Option<String> {
    preset
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn revision_declares_inference(args: &RevisionCreateFromIntentArgs) -> bool {
    normalized_llm_preset(&args.llm_preset).is_some() || args.llm_config.is_some()
}

#[derive(Debug)]
struct RevisionCreateCommonArgs {
    agent_id: String,
    artifact_id: Option<String>,
    base_revision_id: Option<String>,
    summary: Option<String>,
    metadata: Option<serde_json::Value>,
    manifest_meta: Option<serde_json::Value>,
    source_kind: String,
    source_ref: Option<String>,
    detected_network_hosts: Option<Vec<String>>,
}

#[derive(Debug)]
struct PersistedRevisionResult {
    response: serde_json::Value,
    normalized_lock: RuntimeLock,
}

fn artifact_layers_from_bundle(
    bundle: &ArtifactBundle,
) -> Vec<autonoetic_types::layer::ArtifactLayer> {
    bundle
        .layers
        .iter()
        .map(|l| autonoetic_types::layer::ArtifactLayer {
            layer_id: l.layer_id.clone(),
            name: l.name.clone(),
            mount_path: l.mount_path.clone(),
            digest: l.digest.clone(),
        })
        .collect()
}

fn expected_locked_layers(bundle: &ArtifactBundle) -> Vec<LockedLayerMount> {
    let mut layers: Vec<LockedLayerMount> = bundle
        .layers
        .iter()
        .map(|layer| LockedLayerMount {
            layer_id: layer.layer_id.clone(),
            digest: layer.digest.clone(),
            mount_path: layer.mount_path.clone(),
            // Set to None for comparison purposes. The actual approval_scope is populated
            // by scaffold_runtime_lock_with_scopes() when gateway_dir is available.
            // Here we compare only the immutable layer identity (id + digest + path),
            // not the scope, because scope is resolved separately.
            approval_scope: None,
        })
        .collect();
    layers.sort_by(|a, b| {
        (&a.mount_path, &a.layer_id, &a.digest).cmp(&(&b.mount_path, &b.layer_id, &b.digest))
    });
    layers
}

fn comparable_locked_layers(layers: &[LockedLayerMount]) -> Vec<LockedLayerMount> {
    let mut normalized: Vec<LockedLayerMount> = layers
        .iter()
        .cloned()
        .map(|mut layer| {
            layer.approval_scope = None;
            layer
        })
        .collect();
    normalized.sort_by(|a, b| {
        (&a.mount_path, &a.layer_id, &a.digest).cmp(&(&b.mount_path, &b.layer_id, &b.digest))
    });
    normalized
}

fn parse_agent_owned_lock_sections_strict(
    lock_value: serde_yaml::Value,
) -> anyhow::Result<(Vec<LockedDependencySet>, Vec<LockedArtifact>)> {
    let partial: crate::runtime::install_contract::RuntimeLockPartial =
        serde_yaml::from_value(lock_value.clone())
            .map_err(|e| anyhow::anyhow!("Failed to parse runtime.lock partial shape: {}", e))?;

    let mut dep_parse_errors = Vec::new();
    let agent_deps: Vec<LockedDependencySet> = partial
        .dependencies
        .map(|deps| {
            deps.into_iter()
                .enumerate()
                .filter_map(
                    |(i, v)| match serde_yaml::from_value::<LockedDependencySet>(v) {
                        Ok(d) => Some(d),
                        Err(e) => {
                            dep_parse_errors.push(format!("dependencies[{}]: {}", i, e));
                            None
                        }
                    },
                )
                .collect()
        })
        .unwrap_or_default();

    let mut art_parse_errors = Vec::new();
    let agent_arts: Vec<LockedArtifact> = partial
        .artifacts
        .map(|arts| {
            arts.into_iter()
                .enumerate()
                .filter_map(|(i, v)| match serde_yaml::from_value::<LockedArtifact>(v) {
                    Ok(a) => Some(a),
                    Err(e) => {
                        art_parse_errors.push(format!("artifacts[{}]: {}", i, e));
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    if !dep_parse_errors.is_empty() || !art_parse_errors.is_empty() {
        let mut all_errors = dep_parse_errors;
        all_errors.extend(art_parse_errors);
        return Err(anyhow::anyhow!(
            "runtime.lock parse errors:\n{}\n{}",
            all_errors
                .iter()
                .map(|e| format!("  - {}", e))
                .collect::<Vec<_>>()
                .join("\n"),
            crate::runtime::install_contract::render_runtime_lock_example(),
        ));
    }

    Ok((agent_deps, agent_arts))
}

fn create_revision_from_files(
    common: &RevisionCreateCommonArgs,
    created_by_id: &str,
    gateway_dir: &Path,
    gateway_store: &Arc<crate::scheduler::gateway_store::GatewayStore>,
    bundle: Option<&ArtifactBundle>,
    file_map: &mut BTreeMap<String, Vec<u8>>,
    lock_rel_path: &str,
    parsed_lock: RuntimeLock,
    skill_content: &[u8],
    health_report: Option<&crate::runtime::install_contract::BundleHealthReport>,
    script_entry: Option<&str>,
    config: Option<&GatewayConfig>,
) -> anyhow::Result<PersistedRevisionResult> {
    let expected_layers = bundle.map(expected_locked_layers).unwrap_or_default();
    let normalized_lock = normalize_runtime_lock(parsed_lock);
    let comparable_lock_layers = comparable_locked_layers(&normalized_lock.layers);
    anyhow::ensure!(
        comparable_lock_layers == expected_layers,
        "runtime.lock layer closure does not match artifact layers: runtime.lock has {} layer(s), artifact has {} layer(s)",
        normalized_lock.layers.len(),
        expected_layers.len()
    );

    let canonical_lock_bytes = canonical_runtime_lock_bytes(normalized_lock.clone())?;
    file_map.insert(lock_rel_path.to_string(), canonical_lock_bytes.clone());

    if let Some(entry) = script_entry {
        let normalized_entry = normalize_script_entry(entry);
        if let Some(bytes) = file_map.get(&normalized_entry) {
            let starts_with_shebang = bytes.starts_with(b"#!");
            let is_binary = bytes.starts_with(b"\x7fELF");
            anyhow::ensure!(
                starts_with_shebang || is_binary,
                "script_entry '{}' must start with a shebang line (e.g. #!/usr/bin/env python3) \
                 or be a native binary. This is required for the gateway to execute the script directly.",
                entry
            );
        }
    }

    let manifest_hash = format!("sha256:{}", sha256_hex(skill_content));
    let runtime_lock_hash = format!("sha256:{}", sha256_hex(&canonical_lock_bytes));
    let revision_digest_hex = compute_revision_content_digest_hex(file_map);
    let revision_id = format!("rev_sha256:{}", revision_digest_hex);
    let content_digest = format!("sha256:{}", revision_digest_hex);

    let (signature, signer_id) = match crate::runtime::crypto::GatewayIdentityKey::load_or_generate(
        gateway_dir,
    ) {
        Ok(key) => {
            let sig = key.sign(revision_digest_hex.as_bytes());
            let fp = format!("gateway:{}", key.fingerprint());
            (Some(sig), Some(fp))
        }
        Err(e) => {
            let trust_unsigned = config.map_or(false, |c| c.trust_unsigned_bundles);
            if trust_unsigned {
                tracing::warn!(
                    target: "revision",
                    error = %e,
                    "R+11: Gateway identity key unavailable, proceeding unsigned (trust_unsigned_bundles)"
                );
                (None, None)
            } else {
                return Err(anyhow::anyhow!(
                    "R+11: Failed to load gateway identity key for auto-signing: {}. \
                     Set trust_unsigned_bundles: true in config for local development.",
                    e
                ));
            }
        }
    };

    if let Some(existing_rev) = gateway_store.get_agent_revision(&revision_id)? {
        let _ = materialize_revision_directory(
            gateway_dir,
            &common.agent_id,
            &revision_id,
            file_map,
            script_entry,
        )?;

        // If the existing revision is Archived, resurrect it back to Candidate
        // so it becomes usable again. Otherwise the LLM loops forever: same
        // content → same digest → same "already_exists" response, but the
        // archived revision can't be promoted.
        let status_label = if matches!(
            existing_rev.status,
            autonoetic_types::agent_revision::AgentRevisionStatus::Archived,
        ) {
            let _ = gateway_store.update_agent_revision_status(
                &revision_id,
                autonoetic_types::agent_revision::AgentRevisionStatus::Candidate,
            );
            "reactivated"
        } else {
            "already_exists"
        };

        return Ok(PersistedRevisionResult {
            response: serde_json::json!({
                "ok": true,
                "status": status_label,
                "revision_id": revision_id,
                "content_digest": existing_rev.content_digest,
                "agent_id": common.agent_id,
                "artifact_ref": common.source_ref,
                "agent_ref": format!("{}@{}", common.agent_id, revision_id),
                "short_ref": format!("{}@rev_{}", common.agent_id, existing_rev.short_id),
            }),
            normalized_lock,
        });
    }

    let _revision_dir = materialize_revision_directory(
        gateway_dir,
        &common.agent_id,
        &revision_id,
        file_map,
        script_entry,
    )?;

    let now = chrono::Utc::now().to_rfc3339();

    let base_revision_id = common.base_revision_id.as_ref().map(|value| {
        if let Some(parsed) = autonoetic_types::agent_revision::AgentRef::parse(value) {
            parsed.revision_id
        } else {
            value.to_string()
        }
    });

    let rev = autonoetic_types::agent_revision::AgentRevisionRecord {
        revision_id: revision_id.clone(),
        agent_id: common.agent_id.clone(),
        base_revision_id,
        artifact_id: common.artifact_id.clone(),
        content_digest,
        runtime_lock_hash,
        manifest_hash,
        created_at: now,
        created_by_type: PrincipalKind::AutonoeticAgent.tag().to_string(),
        created_by_id: created_by_id.to_string(),
        source_kind: common.source_kind.clone(),
        source_ref: common.source_ref.clone(),
        origin_node_id: "gateway".to_string(),
        trust_domain: "local".to_string(),
        status: autonoetic_types::agent_revision::AgentRevisionStatus::Candidate,
        metadata_json: {
            let mut md = serde_json::json!({
                "summary": common.summary,
                "metadata": common.metadata,
                "has_unresolved_dependencies": health_report.map(|h| h.has_unresolved_dependencies).unwrap_or(false),
                "dependency_files": health_report.map(|h| h.dependency_files.clone()).unwrap_or_default(),
                "detected_external_imports": health_report.map(|h| h.detected_external_imports.clone()).unwrap_or_default(),
            });
            if let Some(ref m) = common.manifest_meta {
                md["manifest"] = m.clone();
            }
            md
        },
        short_id: String::new(),
        signature,
        signer_id,
        detected_network_hosts: common.detected_network_hosts.clone(),
    };

    let short_id = gateway_store.insert_agent_revision_transactional(&rev)?;

    if let Some(artifact_id) = &common.artifact_id {
        let promo_store = crate::runtime::promotion_store::PromotionStore::new(gateway_dir)?;
        let _ =
            promo_store.reconcile_content_digest_for_revision(artifact_id, &rev.content_digest)?;
    }

    let short_ref = format!("{}@rev_{}", common.agent_id, short_id);

    let mut response_obj = serde_json::json!({
        "ok": true,
        "status": "created",
        "revision_id": revision_id,
        "content_digest": rev.content_digest,
        "agent_ref": format!("{}@{}", common.agent_id, revision_id),
        "short_ref": short_ref,
        "agent_id": common.agent_id,
        "artifact_ref": common.source_ref,
        "next_step": "Use agent.revision.promote to activate this revision"
    });

    if let Some(obj) = response_obj.as_object_mut() {
        if rev.signature.is_some() {
            if let Some(ref signer_id) = rev.signer_id {
                obj.insert("signed_by".to_string(), serde_json::json!(signer_id));
            }
        }
    }

    if let Some(hr) = health_report {
        if let Some(obj) = response_obj.as_object_mut() {
            if hr.has_unresolved_dependencies {
                obj.insert(
                    "has_unresolved_dependencies".to_string(),
                    serde_json::json!(true),
                );
                obj.insert(
                    "dependency_files".to_string(),
                    serde_json::json!(hr.dependency_files),
                );
                obj.insert(
                    "detected_external_imports".to_string(),
                    serde_json::json!(hr.detected_external_imports),
                );
                obj.insert("warnings".to_string(), serde_json::json!(hr.warnings));
            }
        }
    }

    Ok(PersistedRevisionResult {
        response: response_obj,
        normalized_lock,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedRevisionArtifactInput {
    artifact_id: String,
    source_ref: String,
}

fn resolve_revision_artifact_input(
    artifact_id: Option<&str>,
    artifact_ref: Option<&str>,
    session_id: Option<&str>,
    gateway_store: &crate::scheduler::gateway_store::GatewayStore,
) -> anyhow::Result<Option<ResolvedRevisionArtifactInput>> {
    let artifact_id = artifact_id.map(str::trim);
    let artifact_ref = artifact_ref.map(str::trim);

    if matches!(artifact_id, Some("")) {
        return Err(tagged::Tagged::validation(anyhow::anyhow!("artifact_id must not be empty")).into());
    }
    if matches!(artifact_ref, Some("")) {
        return Err(tagged::Tagged::validation(anyhow::anyhow!("artifact_ref must not be empty")).into());
    }

    let Some(direct_artifact_id) = artifact_id.filter(|value| !value.is_empty()) else {
        if let Some(ref_id) = artifact_ref.filter(|value| !value.is_empty()) {
            let sid = session_id
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "artifact_ref '{}' requires session context for scope resolution",
                        ref_id
                    )
                })?;
            if ref_id.starts_with("art_") {
                return Ok(Some(ResolvedRevisionArtifactInput {
                    artifact_id: ref_id.to_string(),
                    source_ref: ref_id.to_string(),
                }));
            }
            let record = gateway_store
                .resolve_artifact_ref_any_scope(ref_id, sid)?
                .ok_or_else(|| {
                    anyhow::anyhow!("artifact_ref '{}' not found, expired, or revoked", ref_id)
                })?;
            return Ok(Some(ResolvedRevisionArtifactInput {
                artifact_id: record.artifact_id,
                source_ref: ref_id.to_string(),
            }));
        }
        return Ok(None);
    };

    if let Some(ref_id) = artifact_ref.filter(|value| !value.is_empty()) {
        if ref_id.starts_with("art_") {
            if ref_id != direct_artifact_id {
                anyhow::ensure!(
                    false,
                    "artifact_ref '{}' does not match artifact_id '{}'",
                    ref_id,
                    direct_artifact_id,
                );
            }
            return Ok(Some(ResolvedRevisionArtifactInput {
                artifact_id: direct_artifact_id.to_string(),
                source_ref: ref_id.to_string(),
            }));
        }
        let sid = session_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "artifact_ref '{}' requires session context for scope resolution",
                    ref_id
                )
            })?;
        let record = gateway_store
            .resolve_artifact_ref_any_scope(ref_id, sid)?
            .ok_or_else(|| {
                anyhow::anyhow!("artifact_ref '{}' not found, expired, or revoked", ref_id)
            })?;
        anyhow::ensure!(
            record.artifact_id == direct_artifact_id,
            "artifact_ref '{}' resolves to '{}' but artifact_id '{}' was also provided",
            ref_id,
            record.artifact_id,
            direct_artifact_id,
        );
        return Ok(Some(ResolvedRevisionArtifactInput {
            artifact_id: direct_artifact_id.to_string(),
            source_ref: ref_id.to_string(),
        }));
    }

    Ok(Some(ResolvedRevisionArtifactInput {
        artifact_id: direct_artifact_id.to_string(),
        source_ref: direct_artifact_id.to_string(),
    }))
}

pub struct AgentRevisionCreateTool;

impl NativeTool for AgentRevisionCreateTool {
    fn name(&self) -> &'static str {
        "agent_revision_create"
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
                    "artifact_id": { "type": "string", "description": "Artifact ref (ar.*) containing the agent bundle (SKILL.md + files)" },
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
        let decision = policy.can_agent_revision(&args.agent_id);
        if !decision.is_allowed() {
            return Err(tagged::Tagged::permission_with_rules(
                anyhow::anyhow!(
                    "Permission Denied: agent '{}' lacks AgentRevision capability for '{}'",
                    manifest.agent.id,
                    args.agent_id
                ),
                decision
                    .enforced_rules
                    .into_iter()
                    .map(|rule| rule.to_string())
                    .collect(),
            )
            .into());
        }
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
            bundle.kind == ArtifactKind::AgentBundle,
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

        let Some(skill_content) = file_map.get("SKILL.md").cloned() else {
            let skill_missing = vec!["SKILL.md at artifact root".to_string()];
            return Err(anyhow::anyhow!(
                "{}",
                crate::runtime::install_contract::format_install_validation_error(
                    &skill_missing,
                    None,
                    None
                )
            ));
        };
        let skill_text = String::from_utf8_lossy(&skill_content);

        let frontmatter_value =
            match crate::runtime::install_contract::extract_frontmatter_raw(&skill_text) {
                Ok(v) => v,
                Err(e) => {
                    let parse_error = e.to_string();
                    let skill_missing = vec!["YAML frontmatter".to_string()];
                    return Err(anyhow::anyhow!(
                        "{}",
                        crate::runtime::install_contract::format_install_validation_error(
                            &skill_missing,
                            None,
                            Some(&parse_error)
                        )
                    ));
                }
            };
        let skill_missing =
            crate::runtime::install_contract::validate_skill_frontmatter_shape(&frontmatter_value);
        if !skill_missing.is_empty() {
            let lock_missing_for_error: Option<&[String]> = None;
            return Err(anyhow::anyhow!(
                "{}",
                crate::runtime::install_contract::format_install_validation_error(
                    &skill_missing,
                    lock_missing_for_error,
                    None,
                )
            ));
        }

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

        if let Err(err) = crate::runtime::network_host_contract::validate_network_host_contract(
            &bundle_manifest.capabilities,
            &file_map,
            bundle_manifest.open_web,
        ) {
            return Ok(err.to_error_response());
        }
        let detected_network_hosts =
            crate::runtime::network_host_contract::detect_network_hosts_from_file_map(&file_map);

        let lock_rel_path = bundle_manifest.runtime.runtime_lock.clone();
        validate_relative_agent_path(&lock_rel_path)?;
        let lock_content = file_map.get(&lock_rel_path);

        let parsed_lock: RuntimeLock = if let Some(lock_content) = lock_content {
            let lock_value: serde_yaml::Value =
                serde_yaml::from_slice(lock_content).map_err(|e| {
                    anyhow::anyhow!("Failed to parse '{}' as YAML: {}", lock_rel_path, e)
                })?;

            let lock_missing =
                crate::runtime::install_contract::validate_runtime_lock_shape(&lock_value);
            if !lock_missing.is_empty() {
                return Err(anyhow::anyhow!(
                    "{}",
                    crate::runtime::install_contract::format_install_validation_error(
                        &[],
                        Some(&lock_missing),
                        None,
                    )
                ));
            }

            let (agent_deps, agent_arts) = parse_agent_owned_lock_sections_strict(lock_value)?;
            let scaffolded = crate::runtime::install_contract::scaffold_runtime_lock_with_scopes(
                Some(agent_deps),
                Some(agent_arts),
                &artifact_layers_from_bundle(&bundle),
                Some(gateway_dir),
                None,
            )?;
            scaffolded
        } else {
            crate::runtime::install_contract::scaffold_runtime_lock_with_scopes(
                None,
                None,
                &artifact_layers_from_bundle(&bundle),
                Some(gateway_dir),
                None,
            )?
        };

        let manifest_meta = serde_json::json!({
            "description": bundle_manifest.agent.description,
            "capabilities": bundle_manifest.capabilities.iter().map(crate::runtime::tools::capability_type_name).collect::<Vec<_>>(),
            "execution_mode": match bundle_manifest.execution_mode {
                autonoetic_types::agent::ExecutionMode::Reasoning => "reasoning",
                autonoetic_types::agent::ExecutionMode::Script => "script",
            },
            "script_input_mode": serde_json::to_value(&bundle_manifest.script_input_mode).ok(),
            "script_entry": bundle_manifest.script_entry,
            "io": bundle_manifest.io,
        });
        let common = RevisionCreateCommonArgs {
            agent_id: args.agent_id.clone(),
            artifact_id: Some(args.artifact_id.clone()),
            base_revision_id: args.base_revision_id.clone(),
            summary: args.summary.clone(),
            metadata: args.metadata.clone(),
            manifest_meta: Some(manifest_meta),
            source_kind: "artifact".to_string(),
            source_ref: Some(args.artifact_id.clone()),
            detected_network_hosts: Some(detected_network_hosts),
        };
        let persisted = create_revision_from_files(
            &common,
            &manifest.agent.id,
            gateway_dir,
            &gateway_store,
            Some(&bundle),
            &mut file_map,
            &lock_rel_path,
            parsed_lock,
            &skill_content,
            None,
            bundle_manifest.script_entry.as_deref(),
            _config,
        )?;
        Ok(persisted.response.to_string())
    }
}

/// JSON Schema `oneOf` branches for the `Capability` enum, mirroring
/// `#[serde(tag = "type", deny_unknown_fields)]`. Surfacing each variant's
/// required fields in the tool definition stops the LLM from omitting
/// mandatory fields like `SandboxFunctions.allowed` (it previously only saw
/// `"type": "array"` + prose examples).
///
/// Kept in sync with `autonoetic_types::capability::Capability` — the
/// `capability_oneof_schema_covers_all_variants` test guards against drift.
fn capability_oneof_schema() -> serde_json::Value {
    serde_json::json!([
        {
            "type": "object",
            "description": "MCP tool access by prefix. Tool names are mcp_<server>_<tool>, so prefix-match on that form.",
            "properties": {
                "type": { "const": "SandboxFunctions" },
                "allowed": { "type": "array", "items": { "type": "string" }, "description": "MCP tool prefixes (prefix-matched, trailing * optional), e.g. [\"mcp_web_*\",\"mcp_docs_fetch\",\"mcp_*\"]" }
            },
            "required": ["type", "allowed"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Read access to content/memory/knowledge (includes search).",
            "properties": {
                "type": { "const": "ReadAccess" },
                "scopes": { "type": "array", "items": { "type": "string" }, "description": "e.g. [\"self.*\",\"content.*\"]" }
            },
            "required": ["type", "scopes"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Write access to content/memory/knowledge (includes share).",
            "properties": {
                "type": { "const": "WriteAccess" },
                "scopes": { "type": "array", "items": { "type": "string" }, "description": "e.g. [\"self.*\",\"content.*\"]" }
            },
            "required": ["type", "scopes"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "HTTP/network access — escapes the sandbox boundary.",
            "properties": {
                "type": { "const": "NetworkAccess" },
                "hosts": { "type": "array", "items": { "type": "string" }, "description": "Host patterns, e.g. [\"*\"] or [\"api.example.com\"]" }
            },
            "required": ["type", "hosts"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Create child agent sessions.",
            "properties": {
                "type": { "const": "AgentSpawn" },
                "max_children": { "type": "integer", "minimum": 0, "description": "Limit on concurrent children." },
                "max_spawn_depth": { "type": "integer", "minimum": 0, "default": 0, "description": "Max spawn chain depth (0 = system default)." }
            },
            "required": ["type", "max_children"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Send messages to other agents.",
            "properties": {
                "type": { "const": "AgentMessage" },
                "patterns": { "type": "array", "items": { "type": "string" }, "default": ["*"] }
            },
            "required": ["type"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Periodic wake-ups for background processing.",
            "properties": {
                "type": { "const": "BackgroundReevaluation" },
                "min_interval_secs": { "type": "integer", "minimum": 1 },
                "allow_reasoning": { "type": "boolean" }
            },
            "required": ["type", "min_interval_secs", "allow_reasoning"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Execute scripts/code in the sandbox.",
            "properties": {
                "type": { "const": "CodeExecution" },
                "patterns": { "type": "array", "items": { "type": "string" }, "default": ["*"], "description": "Command prefix patterns." },
                "commands": { "type": "array", "items": { "type": "string" }, "default": [], "description": "Specific shell commands (word-boundary match)." }
            },
            "required": ["type"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Request a gateway-level emergency stop (dedicated responders only).",
            "properties": { "type": { "const": "EmergencyStop" } },
            "required": ["type"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Access to agent revision operations (create, promote, rollback).",
            "properties": {
                "type": { "const": "AgentRevision" },
                "patterns": { "type": "array", "items": { "type": "string" }, "default": ["*"] }
            },
            "required": ["type"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Access to evaluation operations (suite publish, run, report).",
            "properties": {
                "type": { "const": "Evaluation" },
                "patterns": { "type": "array", "items": { "type": "string" }, "default": ["*"] }
            },
            "required": ["type"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Access to approval queue operations.",
            "properties": {
                "type": { "const": "ApprovalQueue" },
                "patterns": { "type": "array", "items": { "type": "string" }, "default": ["*"] }
            },
            "required": ["type"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Access to scheduler signal operations.",
            "properties": {
                "type": { "const": "SchedulerSignal" },
                "patterns": { "type": "array", "items": { "type": "string" }, "default": ["*"] }
            },
            "required": ["type"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Access to credential operations (check, request, setup).",
            "properties": {
                "type": { "const": "CredentialAccess" },
                "services": { "type": "array", "items": { "type": "string" }, "default": ["*"], "description": "e.g. [\"github\"]" }
            },
            "required": ["type"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Access to user profile operations (read, update, share, revoke).",
            "properties": {
                "type": { "const": "UserProfileAccess" },
                "scopes": { "type": "array", "items": { "type": "string" }, "description": "e.g. [\"read\"] or [\"read\",\"write\"]" }
            },
            "required": ["type", "scopes"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Access to scheduler/cron operations.",
            "properties": {
                "type": { "const": "SchedulerAccess" },
                "patterns": { "type": "array", "items": { "type": "string" }, "default": ["*"] }
            },
            "required": ["type"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Install a remote SKILL.md as a new local agent.",
            "properties": {
                "type": { "const": "SkillInstall" },
                "allowed_sources": { "type": "array", "items": { "type": "string" }, "default": ["*"], "description": "Permitted URL hosts." }
            },
            "required": ["type"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Submit constitutional amendment proposals.",
            "properties": {
                "type": { "const": "ConstitutionalProposal" },
                "patterns": { "type": "array", "items": { "type": "string" }, "default": ["*"] }
            },
            "required": ["type"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Read reasoning traces from other agents' sessions.",
            "properties": {
                "type": { "const": "ReasoningAudit" },
                "targets": { "type": "array", "items": { "type": "string" }, "default": ["*"] }
            },
            "required": ["type"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Allow running with max_session_price_usd while model price metadata is unavailable.",
            "properties": { "type": { "const": "budget.no_price_available.allow" } },
            "required": ["type"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Create GitHub issues.",
            "properties": {
                "type": { "const": "GithubIssueCreate" },
                "patterns": { "type": "array", "items": { "type": "string" }, "default": ["*"] }
            },
            "required": ["type"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Submit adversarial attack-pattern proposals (system-tier red-team only).",
            "properties": { "type": { "const": "SecurityRedTeam" } },
            "required": ["type"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Export/import Cognitive Capsules across machines or gateways.",
            "properties": { "type": { "const": "CapsuleExport" } },
            "required": ["type"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Propose new wiki pages to be curated into the platform wiki.",
            "properties": { "type": { "const": "WikiContribute" } },
            "required": ["type"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Access to PlanFrame operations (propose, amend, approve, list, get).",
            "properties": {
                "type": { "const": "PlanFrameAccess" },
                "patterns": { "type": "array", "items": { "type": "string" }, "default": ["*"] }
            },
            "required": ["type"],
            "additionalProperties": false
        },
        {
            "type": "object",
            "description": "Pre-authorizes promotion of an agent whose declared capabilities fall within this set.",
            "properties": {
                "type": { "const": "PromoteWith" },
                "agent_id": { "type": "string", "default": "" },
                "capabilities": { "type": "array", "items": { "$ref": "#/$defs/Capability" } }
            },
            "required": ["type", "capabilities"],
            "additionalProperties": false
        }
    ])
}

pub struct AgentRevisionCreateFromIntentTool;

impl NativeTool for AgentRevisionCreateFromIntentTool {
    fn name(&self) -> &'static str {
        "agent_revision_create_from_intent"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::AgentRevision { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        let capability_branches = capability_oneof_schema();
        ToolDefinition {
            name: self.name().to_string(),
            description: "Create a new immutable agent revision from semantic intent, canonicalizing SKILL.md and runtime.lock server-side. For pure reasoning agents that only use existing gateway tools (no custom code), omit artifact_ref — capability enforcement is the security gate. For script agents or agents with CodeExecution/AgentSpawn, pass the artifact_ref returned by artifact_build.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string" },
                    "artifact_ref": { "type": "string", "description": "Artifact ref from artifact_build (e.g., 'ar.aabb1234ef56'). Resolved against the caller's accessible scopes." },
                    "instructions": { "type": "string", "description": "Markdown instruction body for SKILL.md" },
                    "description": { "type": "string", "description": "Agent description for metadata.agent.description" },
                    "execution_mode": { "type": "string", "enum": ["reasoning", "script"] },
                    "script_entry": { "type": "string" },
                    "script_input_mode": { "type": "string", "enum": ["stdin", "args"], "description": "How normalized task input is delivered to script agents: 'stdin' (default) writes the payload to stdin; 'args' passes the same payload as the first positional CLI argument ($1)." },
                    "llm_preset": { "type": "string", "description": "Named preset in gateway llm_presets (preferred for reasoning agents)" },
                    "llm_overrides": { "type": "object", "description": "Optional temperature/thinking overrides merged onto the resolved preset" },
                    "llm_config": { "type": "object", "description": "Legacy inline provider/model — prefer llm_preset" },
                    "capabilities": {
                        "type": "array",
                        "description": "Each item is a tagged Capability object (bare strings are rejected). See $defs/Capability for each variant's required fields.",
                        "items": { "$ref": "#/$defs/Capability" }
                    },
                    "io": {
                        "type": "object",
                        "description": "I/O contract. Declare accepts (input JSON schema), returns (output JSON schema), and optional output_policy (runtime output constraints). Example: {\"accepts\":{\"type\":\"object\",\"required\":[\"task\"],\"properties\":{\"task\":{\"type\":\"string\"}}},\"returns\":{\"type\":\"object\"},\"output_policy\":{\"max_reply_length_chars\":2000}}"
                    },
                    "middleware": { "type": "object" },
                    "base_revision_id": { "type": "string" },
                    "summary": { "type": "string" },
                    "replace": { "type": "boolean", "description": "Set to true to archive the existing active revision and install this as the new one. Required when updating an already-installed agent." },
                    "credential_services": { "type": "array", "items": { "type": "string" }, "description": "Service names whose credentials the agent needs at spawn time. The env-var name is derived deterministically from the service name (e.g. 'moltbook' → MOLTBOOK_SECRET). Only meaningful for script-mode agents." }
                },
                "required": ["agent_id", "instructions", "description", "capabilities"],
                "additionalProperties": false,
                "$defs": {
                    "Capability": { "oneOf": capability_branches }
                }
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
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: RevisionCreateFromIntentArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments: {}", e))?;

        crate::runtime::tools::validate_agent_id(&args.agent_id)?;
        anyhow::ensure!(
            !args.instructions.trim().is_empty(),
            "instructions must not be empty"
        );
        anyhow::ensure!(
            !args.description.trim().is_empty(),
            "description must not be empty"
        );
        let decision = policy.can_agent_revision(&args.agent_id);
        if !decision.is_allowed() {
            return Err(tagged::Tagged::permission_with_rules(
                anyhow::anyhow!(
                    "Permission Denied: agent '{}' lacks AgentRevision capability for '{}'",
                    manifest.agent.id,
                    args.agent_id
                ),
                decision
                    .enforced_rules
                    .into_iter()
                    .map(|rule| rule.to_string())
                    .collect(),
            )
            .into());
        }
        if matches!(args.execution_mode, Some(ExecutionMode::Reasoning))
            && !revision_declares_inference(&args)
        {
            return Ok(ToolError::validation(
                "llm_preset or llm_config is required when execution_mode is 'reasoning'",
                None::<String>,
            )
            .to_error_response());
        }

        let Some(gateway_store) = gateway_store else {
            return Err(anyhow::anyhow!(
                "GatewayStore is required for revision creation"
            ));
        };
        let gateway_dir = gateway_dir.ok_or_else(|| anyhow::anyhow!("gateway_dir required"))?;

        let resolved_artifact = resolve_revision_artifact_input(
            args.artifact_id.as_deref(),
            args.artifact_ref.as_deref(),
            session_id,
            &gateway_store,
        )?;

        let single_flight_scope = if let (Some(config), Some(session_id)) = (_config, session_id) {
            let root_session_id = crate::runtime::content_store::root_session_id(session_id);
            let workflow = crate::scheduler::ensure_workflow_for_root_session(
                config,
                Some(gateway_store.as_ref()),
                &root_session_id,
                Some(manifest.agent.id.as_str()),
            )?;
            let spec = crate::scheduler::single_flight::build_spec(
                &workflow.workflow_id,
                "install",
                &args.agent_id,
                resolved_artifact
                    .as_ref()
                    .map(|artifact| artifact.source_ref.clone()),
                resolved_artifact
                    .as_ref()
                    .map(|_| None)
                    .unwrap_or_else(|| Some(direct_install_intent_digest(&args))),
            );
            match crate::scheduler::single_flight::try_acquire_reservation(
                config,
                Some(gateway_store.as_ref()),
                &spec,
                None,
                None,
            )? {
                crate::scheduler::single_flight::AcquireOutcome::Acquired(_) => {
                    Some((workflow.workflow_id, spec))
                }
                crate::scheduler::single_flight::AcquireOutcome::Coalesced(existing) => {
                    crate::scheduler::append_workflow_event(
                        config,
                        Some(gateway_store.as_ref()),
                        &autonoetic_types::workflow::WorkflowEventRecord {
                            event_id: format!("wevt-{}", &uuid::Uuid::new_v4().to_string()[..8]),
                            workflow_id: workflow.workflow_id.clone(),
                            task_id: existing.existing_task_id.clone(),
                            event_type: "workflow.single_flight.coalesced".to_string(),
                            agent_id: Some(args.agent_id.clone()),
                            payload: serde_json::json!({
                                "status": "coalesced",
                                "stage_kind": existing.stage_kind,
                                "dedupe_key": existing.dedupe_key,
                                "existing_task_id": existing.existing_task_id,
                                "approval_request_id": existing.approval_request_id,
                                "retry_advice": "wait",
                            }),
                            occurred_at: chrono::Utc::now().to_rfc3339(),
                        },
                    )?;
                    return Ok(serde_json::json!({
                        "ok": true,
                        "status": "coalesced",
                        "existing_task_id": existing.existing_task_id,
                        "approval_ref": existing.approval_request_id,
                        "dedupe_key": existing.dedupe_key,
                        "retry_advice": "wait",
                        "message": "Equivalent durable install operation is already active. Wait for the existing operation instead."
                    })
                    .to_string());
                }
            }
        } else {
            None
        };
        let _single_flight_guard = single_flight_scope
            .as_ref()
            .and_then(|(workflow_id, spec)| {
                _config.map(|config| {
                    SingleFlightReleaseGuard::new(
                        config,
                        workflow_id.clone(),
                        spec.dedupe_key.clone(),
                    )
                })
            });

        let existing = gateway_store.list_agent_revisions(&args.agent_id)?;
        let ready_revisions: Vec<String> = existing
            .iter()
            .filter(|rev| {
                matches!(
                    rev.status,
                    autonoetic_types::agent_revision::AgentRevisionStatus::Ready,
                )
            })
            .map(|r| r.revision_id.clone())
            .collect();
        // Auto-archive any Candidate (never-promoted) revisions — they are
        // aborted install attempts and should not block a fresh install.
        for rev in &existing {
            if matches!(
                rev.status,
                autonoetic_types::agent_revision::AgentRevisionStatus::Candidate,
            ) {
                let _ = gateway_store.update_agent_revision_status(
                    &rev.revision_id,
                    autonoetic_types::agent_revision::AgentRevisionStatus::Archived,
                );
            }
        }
        if !ready_revisions.is_empty() {
            if args.replace {
                for rev_id in &ready_revisions {
                    gateway_store
                        .update_agent_revision_status(
                            rev_id,
                            autonoetic_types::agent_revision::AgentRevisionStatus::Archived,
                        )
                        .map_err(|e| {
                            anyhow::anyhow!("Failed to archive existing revision {}: {}", rev_id, e)
                        })?;
                }
            } else {
                return Ok(ToolError::fatal(
                    format!(
                        "Agent '{}' already has a promoted revision ({}). \
                         Use agent_revision_list / agent_revision_inspect (or agent_inspect if you hold ReadAccess) to check before installing. \
                         If you need to update, pass replace: true to archive the existing revision first.",
                        args.agent_id,
                        ready_revisions.join(", ")
                    ),
                    None::<String>,
                ).to_error_response());
            }
        }

        // Two execution paths: with artifact (code agents) vs without (pure reasoning agents).
        let (
            resolved_mode,
            resolved_script_entry,
            mut file_map,
            health_report,
            bundle_opt,
            source_kind,
            source_ref,
            detected_network_hosts,
        ) = if let Some(resolved_artifact) = resolved_artifact.as_ref() {
            let artifact_id = &resolved_artifact.artifact_id;
            anyhow::ensure!(
                !artifact_id.trim().is_empty(),
                "artifact_id must not be empty"
            );
            let artifact_store = crate::ArtifactStore::new(gateway_dir)?;
            let bundle = artifact_store
                .inspect(artifact_id)
                .map_err(|e| anyhow::anyhow!("Artifact '{}' not found: {}", artifact_id, e))?;
            let has_entrypoint = !bundle.entrypoints.is_empty()
                || args
                    .script_entry
                    .as_ref()
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false);
            anyhow::ensure!(
                    bundle.kind == ArtifactKind::AgentBundle
                        || (bundle.kind == ArtifactKind::Binary && has_entrypoint),
                    "Artifact '{}' has kind '{:?}'. agent.revision.create_from_intent requires kind 'agent_bundle', or 'binary' with an entrypoint.",
                    artifact_id,
                    bundle.kind
                );

            let requested_mode = args.execution_mode.unwrap_or(ExecutionMode::Reasoning);
            let execution_mode_explicit = args.execution_mode.is_some();
            let resolved_mode = if !execution_mode_explicit
                && requested_mode == ExecutionMode::Reasoning
                && args.script_entry.is_none()
                && !revision_declares_inference(&args)
            {
                if bundle.entrypoints.len() == 1 {
                    tracing::info!(
                        target: "revision",
                        artifact_id = %artifact_id,
                        entrypoint = %bundle.entrypoints[0],
                        "No execution_mode or llm_config specified, but artifact has a single entrypoint — defaulting to script mode"
                    );
                    ExecutionMode::Script
                } else {
                    ExecutionMode::Reasoning
                }
            } else {
                requested_mode
            };
            match resolved_mode {
                ExecutionMode::Script => {
                    let has_entry = args
                        .script_entry
                        .as_ref()
                        .map(|v| !v.trim().is_empty())
                        .unwrap_or(false)
                        || !bundle.entrypoints.is_empty();
                    anyhow::ensure!(
                        has_entry,
                        "script_entry is required when execution_mode is 'script'"
                    );
                }
                ExecutionMode::Reasoning => anyhow::ensure!(
                    revision_declares_inference(&args),
                    "llm_preset or llm_config is required when execution_mode is 'reasoning'"
                ),
            }

            let resolved_script_entry = args
                .script_entry
                .as_ref()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .or_else(|| {
                    if resolved_mode == ExecutionMode::Script {
                        bundle.entrypoints.clone().into_iter().next()
                    } else {
                        None
                    }
                })
                .map(|e| normalize_script_entry(&e));

            let mut file_map: BTreeMap<String, Vec<u8>> = BTreeMap::new();
            let strip_prefix = format!("{}/", args.agent_id);
            for (path, bytes) in artifact_store.resolve_files(artifact_id)? {
                validate_relative_agent_path(&path)?;
                let normalized_path = path.strip_prefix(&strip_prefix).unwrap_or(&path);
                anyhow::ensure!(
                    file_map
                        .insert(normalized_path.to_string(), bytes)
                        .is_none(),
                    "Artifact contains duplicate file path '{}'",
                    normalized_path
                );
            }

            let has_layers = !bundle.layers.is_empty();
            let health_report = crate::runtime::install_contract::analyze_bundle_health(
                &file_map,
                &args.capabilities,
                has_layers,
                resolved_script_entry.as_deref(),
            );

            if let Err(err) = crate::runtime::network_host_contract::validate_network_host_contract(
                &args.capabilities,
                &file_map,
                args.open_web,
            ) {
                return Ok(err.to_error_response());
            }
            let detected_network_hosts =
                crate::runtime::network_host_contract::detect_network_hosts_from_file_map(&file_map);

            (
                resolved_mode,
                resolved_script_entry,
                file_map,
                Some(health_report),
                Some(bundle),
                "intent_artifact".to_string(),
                Some(resolved_artifact.source_ref.clone()),
                Some(detected_network_hosts),
            )
        } else {
            // Pure reasoning agent — no artifact, no custom code.
            // Validate that this path is safe: no script mode, no CodeExecution/AgentSpawn.
            anyhow::ensure!(
                    !matches!(args.execution_mode, Some(ExecutionMode::Script)),
                    "execution_mode 'script' requires an artifact_ref or artifact_id — script agents must have source files"
                );
            anyhow::ensure!(
                args.script_entry.is_none(),
                "script_entry requires an artifact_ref or artifact_id — pure reasoning agents have no scripts"
            );
            anyhow::ensure!(
                revision_declares_inference(&args),
                "llm_preset or llm_config is required for pure reasoning agents (no artifact_ref/artifact_id)"
            );
            let forbidden_cap = args
                .capabilities
                .iter()
                .find(|cap| crate::runtime::install_contract::requires_artifact_review(cap));
            if let Some(cap) = forbidden_cap {
                return Ok(ToolError::validation(
                    format!(
                        "Capability '{:?}' requires an artifact_ref or artifact_id for code review and promotion gating. \
                         Pure reasoning agents (no artifact_id) may not use CodeExecution or AgentSpawn.",
                        cap
                    ),
                    None::<String>,
                ).to_error_response());
            }

            (
                ExecutionMode::Reasoning,
                None,
                BTreeMap::new(),
                None,
                None,
                "intent_reasoning".to_string(),
                None,
                None,
            )
        };

        let artifact_layers: Vec<autonoetic_types::layer::ArtifactLayer> = bundle_opt
            .as_ref()
            .map(|b| artifact_layers_from_bundle(b))
            .unwrap_or_default();

        let target_manifest = AgentManifest {
            version: "1.0".to_string(),
            runtime: crate::runtime::install_contract::default_runtime_declaration(),
            agent: AgentIdentity {
                id: args.agent_id.clone(),
                name: args.agent_id.clone(),
                description: args.description.clone(),
            },
            capabilities: args.capabilities.clone(),
            llm_preset: normalized_llm_preset(&args.llm_preset),
            llm_overrides: args.llm_overrides.clone(),
            llm_config: if normalized_llm_preset(&args.llm_preset).is_some() {
                None
            } else {
                args.llm_config.clone()
            },
            limits: None,
            background: None,
            disclosure: None,
            io: args.io.clone(),
            middleware: args.middleware.clone(),
            execution_mode: resolved_mode,
            script_entry: resolved_script_entry.clone(),
            script_input_mode: args.script_input_mode.unwrap_or_default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            agentskills_import: None,
            compression: None,
            open_web: args.open_web,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        };

        let canonical_skill = crate::runtime::install_contract::render_skill_document(
            &target_manifest,
            &args.instructions,
        )?;
        let skill_content = canonical_skill.as_bytes().to_vec();

        file_map.insert("SKILL.md".to_string(), skill_content.clone());

        let lock_rel_path = target_manifest.runtime.runtime_lock.clone();
        validate_relative_agent_path(&lock_rel_path)?;
        let parsed_lock = crate::runtime::install_contract::scaffold_runtime_lock_with_scopes(
            None,
            None,
            &artifact_layers,
            Some(gateway_dir),
            if args.credential_services.is_empty() { None } else { Some(args.credential_services.clone()) },
        )?;

        let manifest_meta = serde_json::json!({
            "description": target_manifest.agent.description,
            "capabilities": target_manifest.capabilities.iter().map(crate::runtime::tools::capability_type_name).collect::<Vec<_>>(),
            "execution_mode": match target_manifest.execution_mode {
                autonoetic_types::agent::ExecutionMode::Reasoning => "reasoning",
                autonoetic_types::agent::ExecutionMode::Script => "script",
            },
            "script_input_mode": serde_json::to_value(&target_manifest.script_input_mode).ok(),
            "script_entry": target_manifest.script_entry,
            "io": target_manifest.io,
        });
        let common = RevisionCreateCommonArgs {
            agent_id: args.agent_id.clone(),
            artifact_id: resolved_artifact
                .as_ref()
                .map(|artifact| artifact.artifact_id.clone()),
            base_revision_id: args.base_revision_id.clone(),
            summary: args.summary.clone(),
            metadata: args.metadata.clone(),
            manifest_meta: Some(manifest_meta),
            source_kind,
            source_ref,
            detected_network_hosts,
        };
        let persisted = create_revision_from_files(
            &common,
            &manifest.agent.id,
            gateway_dir,
            &gateway_store,
            bundle_opt.as_ref(),
            &mut file_map,
            &lock_rel_path,
            parsed_lock,
            &skill_content,
            health_report.as_ref(),
            resolved_script_entry.as_deref(),
            _config,
        )?;

        let mut response = persisted.response;
        let normalized_lock = serde_json::to_value(&persisted.normalized_lock)?;
        let canonical_skill_metadata = serde_json::to_value(&target_manifest)?;
        if let Some(obj) = response.as_object_mut() {
            obj.insert(
                "canonical_skill_metadata".to_string(),
                canonical_skill_metadata,
            );
            obj.insert("canonical_runtime_lock".to_string(), normalized_lock);
            obj.insert(
                "autofilled_fields".to_string(),
                serde_json::json!([
                    "runtime.engine",
                    "runtime.gateway_version",
                    "runtime.sdk_version",
                    "runtime.runtime_lock",
                    "runtime.lock.gateway",
                    "runtime.lock.sdk",
                    "runtime.lock.sandbox",
                    "runtime.lock.layers"
                ]),
            );
            obj.insert(
                "normalized_fields".to_string(),
                serde_json::json!([
                    "runtime.lock.dependencies",
                    "runtime.lock.artifacts",
                    "runtime.lock.layers"
                ]),
            );
        }
        Ok(response.to_string())
    }
}

pub struct AgentRevisionSchemaTool;

impl NativeTool for AgentRevisionSchemaTool {
    fn name(&self) -> &'static str {
        "agent_revision_schema"
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Returns the install contract schema for agent revision creation — ownership split, required fields, and canonical examples.".to_string(),
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
        _gateway_dir: Option<&Path>,
        _arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&GatewayConfig>,
        _gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        Ok(serde_json::json!({
            "ok": true,
            "schema": crate::runtime::install_contract::install_schema_description(),
            "skill_example": crate::runtime::install_contract::render_skill_metadata_example(),
            "lock_example": crate::runtime::install_contract::render_runtime_lock_example(),
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
        "agent_revision_list"
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
            let decision = policy.can_agent_revision(agent_id);
            if !decision.is_allowed() {
                return Err(tagged::Tagged::permission_with_rules(
                    anyhow::anyhow!(
                        "Permission Denied: missing AgentRevision capability for '{}'",
                        agent_id
                    ),
                    decision
                        .enforced_rules
                        .into_iter()
                        .map(|rule| rule.to_string())
                        .collect(),
                )
                .into());
            }
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
                    "artifact_ref": r.source_ref,
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
        "agent_revision_inspect"
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
        let decision = policy.can_agent_revision(&rev.agent_id);
        if !decision.is_allowed() {
            return Err(tagged::Tagged::permission_with_rules(
                anyhow::anyhow!(
                    "Permission Denied: missing AgentRevision capability for '{}'",
                    rev.agent_id
                ),
                decision
                    .enforced_rules
                    .into_iter()
                    .map(|rule| rule.to_string())
                    .collect(),
            )
            .into());
        }

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

struct SingleFlightReleaseGuard<'a> {
    config: &'a GatewayConfig,
    workflow_id: String,
    dedupe_key: String,
    armed: bool,
}

impl<'a> SingleFlightReleaseGuard<'a> {
    fn new(config: &'a GatewayConfig, workflow_id: String, dedupe_key: String) -> Self {
        Self {
            config,
            workflow_id,
            dedupe_key,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SingleFlightReleaseGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = crate::scheduler::single_flight::release_reservation(
                self.config,
                &self.workflow_id,
                &self.dedupe_key,
            );
        }
    }
}

fn normalized_install_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn direct_install_intent_digest(args: &RevisionCreateFromIntentArgs) -> String {
    let normalized = serde_json::json!({
        "agent_id": args.agent_id,
        "instructions": normalized_install_text(&args.instructions),
        "description": normalized_install_text(&args.description),
        "capabilities": args.capabilities,
        "execution_mode": args.execution_mode,
        "script_entry": args.script_entry.as_deref().map(normalized_install_text),
        "script_input_mode": args.script_input_mode,
        "llm_config": args.llm_config,
        "io": args.io,
        "middleware": args.middleware,
        "base_revision_id": args.base_revision_id,
        "replace": args.replace,
        "credential_services": args.credential_services,
    });
    format!(
        "intent:{}",
        sha256_hex(serde_json::to_string(&normalized).unwrap_or_default().as_bytes())
    )
}

#[derive(Debug, Deserialize)]
struct RevisionPromoteArgs {
    agent_id: String,
    revision_id: String,
    reason: Option<String>,
    required_eval_run_id: Option<String>,
    /// Approval reference returned by an earlier promote call that hit the
    /// capability-delta gate (R++2). When supplied and approved, the gate is
    /// bypassed for this exact (agent_id, revision_id) pair.
    #[serde(default)]
    approval_ref: Option<String>,
    /// Bypass the promotion safety governor (issue #25). Requires
    /// `force_reason` and emits a `governor.override` causal event.
    #[serde(default)]
    force: Option<bool>,
    /// Operator-supplied justification recorded with the override event.
    /// Required when `force = true`.
    #[serde(default)]
    force_reason: Option<String>,
    /// Task id of a successful smoke-test run for this candidate revision.
    /// Required when `agent_install_smoke_test` is `required` for new agents.
    #[serde(default)]
    smoke_test_task_id: Option<String>,
    /// Workflow id containing the smoke-test task.
    #[serde(default)]
    smoke_test_workflow_id: Option<String>,
}

pub struct AgentRevisionPromoteTool;

impl NativeTool for AgentRevisionPromoteTool {
    fn name(&self) -> &'static str {
        "agent_revision_promote"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::AgentRevision { .. }))
    }

    fn guidance(&self) -> Vec<crate::runtime::guidance::GuidanceBlock> {
        use crate::runtime::guidance::{GuidanceBlock, GuidanceCondition};
        vec![GuidanceBlock {
            id: "promote.approval_continuation",
            when: GuidanceCondition::ToolPresent("agent_revision_promote"),
            priority: 9,
            prose: "**Promotion resumes automatically — don't loop.** If `agent_revision_promote` \
returns `approval_required: true` (e.g. `capability_delta_requires_approval`), return the exact \
`request_id`/`approval_ref` to your caller and end your turn. The gateway wakes you when the operator \
decides and re-runs the promotion for you with the real result — do **not** re-spawn the builder, \
re-run the gates, or re-issue the promote. A locked session capability envelope (PromoteWith) \
pre-authorizes the capability acknowledgement, so a covered promotion needs no new approval at all. \
On success the response is terminal: `status:\"promoted\"`, `installed:true`. That means the agent is \
now the **active installed revision** — use it by calling `agent_spawn` with its `agent_id`; do not \
rebuild, re-promote, or re-inspect to \"confirm\" it. If it returns `status:\"coalesced\"` with \
`retry_advice:\"wait\"`, an equivalent promote is already running: end your turn and let it finish — \
do not re-issue."
                .to_string(),
        }]
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
                    "required_eval_run_id": { "type": "string", "description": "Optional: if provided, promotion requires this eval run to have passed for the target revision" },
                    "approval_ref": { "type": "string", "description": "Optional: approval ID returned by an earlier promote call that hit the capability-delta gate (R++2). Pass it on retry to bypass the gate." },
                    "force": { "type": "boolean", "description": "Optional: bypass the promotion safety governor (issue #25). Requires `force_reason`; emits a `governor.override` causal event." },
                    "force_reason": { "type": "string", "description": "Required when `force = true`. Operator-supplied justification recorded with the override event." },
                    "smoke_test_task_id": { "type": "string", "description": "Optional: task id of a successful smoke-test run for this candidate revision. Required when agent_install_smoke_test is 'required' for new agents." },
                    "smoke_test_workflow_id": { "type": "string", "description": "Optional: workflow id containing the smoke-test task. Required alongside smoke_test_task_id when agent_install_smoke_test is 'required'." }
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
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        turn_id: Option<&str>,
        config: Option<&GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: RevisionPromoteArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments: {}", e))?;

        crate::runtime::tools::validate_agent_id(&args.agent_id)?;
        let decision = policy.can_agent_revision(&args.agent_id);
        if !decision.is_allowed() {
            return Err(tagged::Tagged::permission_with_rules(
                anyhow::anyhow!(
                    "Permission Denied: missing AgentRevision capability for '{}'",
                    args.agent_id
                ),
                decision
                    .enforced_rules
                    .into_iter()
                    .map(|rule| rule.to_string())
                    .collect(),
            )
            .into());
        }

        let Some(gateway_store) = gateway_store else {
            return Err(anyhow::anyhow!("GatewayStore is required"));
        };
        let gateway_dir = gateway_dir.ok_or_else(|| anyhow::anyhow!("gateway_dir required"))?;

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

        let single_flight_scope = if let (Some(config), Some(session_id)) = (config, session_id) {
            let root_session_id = crate::runtime::content_store::root_session_id(session_id);
            let workflow = crate::scheduler::ensure_workflow_for_root_session(
                config,
                Some(gateway_store.as_ref()),
                &root_session_id,
                Some(manifest.agent.id.as_str()),
            )?;
            let spec = crate::scheduler::single_flight::build_spec(
                &workflow.workflow_id,
                "promote",
                &args.agent_id,
                rev.source_ref.clone().or_else(|| Some(args.revision_id.clone())),
                None,
            );
            match crate::scheduler::single_flight::try_acquire_reservation(
                config,
                Some(gateway_store.as_ref()),
                &spec,
                None,
                args.approval_ref.as_deref(),
            )? {
                crate::scheduler::single_flight::AcquireOutcome::Acquired(_) => {
                    Some((workflow.workflow_id, spec))
                }
                crate::scheduler::single_flight::AcquireOutcome::Coalesced(existing) => {
                    crate::scheduler::append_workflow_event(
                        config,
                        Some(gateway_store.as_ref()),
                        &autonoetic_types::workflow::WorkflowEventRecord {
                            event_id: format!("wevt-{}", &uuid::Uuid::new_v4().to_string()[..8]),
                            workflow_id: workflow.workflow_id.clone(),
                            task_id: existing.existing_task_id.clone(),
                            event_type: "workflow.single_flight.coalesced".to_string(),
                            agent_id: Some(args.agent_id.clone()),
                            payload: serde_json::json!({
                                "status": "coalesced",
                                "stage_kind": existing.stage_kind,
                                "dedupe_key": existing.dedupe_key,
                                "existing_task_id": existing.existing_task_id,
                                "approval_request_id": existing.approval_request_id,
                                "retry_advice": "wait",
                            }),
                            occurred_at: chrono::Utc::now().to_rfc3339(),
                        },
                    )?;
                    return Ok(serde_json::json!({
                        "ok": true,
                        "status": "coalesced",
                        "existing_task_id": existing.existing_task_id,
                        "approval_ref": existing.approval_request_id,
                        "dedupe_key": existing.dedupe_key,
                        "retry_advice": "wait",
                        "message": "Equivalent durable promote operation is already active. Wait for the existing operation instead."
                    })
                    .to_string());
                }
            }
        } else {
            None
        };
        let mut single_flight_guard = single_flight_scope
            .as_ref()
            .and_then(|(workflow_id, spec)| {
                config.map(|config| {
                    SingleFlightReleaseGuard::new(
                        config,
                        workflow_id.clone(),
                        spec.dedupe_key.clone(),
                    )
                })
            });

        let revision_dir = gateway_dir
            .join("revisions/agents")
            .join(&args.agent_id)
            .join(&args.revision_id);
        let skill_path = revision_dir.join("SKILL.md");
        let skill_bytes = std::fs::read(&skill_path).map_err(|e| {
            anyhow::anyhow!(
                "Cannot read SKILL.md for revision '{}': {}",
                args.revision_id,
                e
            )
        })?;
        let skill_text = String::from_utf8_lossy(&skill_bytes);
        let skill_frontmatter = crate::runtime::install_contract::extract_frontmatter_raw(
            &skill_text,
        )
        .map_err(|e| {
            anyhow::anyhow!(
                "Cannot parse SKILL.md frontmatter for revision '{}': {}",
                args.revision_id,
                e
            )
        })?;

        let current_capabilities = parse_frontmatter_capabilities(&skill_frontmatter)?;
        let delta_mode = config
            .map(|c| c.capability_delta_gate_mode)
            .unwrap_or(CapabilityDeltaGateMode::Strict);

        // R++2: capability-delta gating. The gate fires on broadening relative
        // to the outgoing revision and is only bypassed by an approved
        // `RevisionPromote` approval whose acknowledgement matches the delta
        // exactly. The `approval_ref` argument is how a retry call reuses an
        // earlier approval.
        // A NEW agent's whole capability set is its "delta", and an approved
        // federation escalation already constitutes operator approval of the
        // entire agent — so it satisfies the new-agent gate without a separate
        // capability-ack approval. (Existing-agent broadening still requires the
        // explicit ack approval.) This keeps the federation promotion path from
        // demanding two operator approvals for one new agent.
        let new_agent_approved_via_escalation =
            match gateway_store.resolve_alias(&args.agent_id)? {
                None => match rev.artifact_id.as_deref() {
                    Some(aid) => {
                        gateway_store
                            .find_escalation(
                                aid,
                                &args.revision_id,
                                autonoetic_types::escalation::EscalationStatus::Approved,
                            )?
                            .is_some()
                            || gateway_store
                                .find_approved_escalation_for_artifact(aid)?
                                .is_some()
                    }
                    None => false,
                },
                Some(_) => false,
            };
        let gate_bypassed_by_approval = new_agent_approved_via_escalation
            || if let Some(ref approval_ref) = args.approval_ref {
                check_revision_promote_approval(
                    &gateway_store,
                    approval_ref,
                    &args.agent_id,
                    &args.revision_id,
                )?
            } else {
                false
            };
        // Set inside the gate bypass block below, but referenced in the
        // response — declared here for scope.
        let mut pre_auth_envelope_id: Option<i64> = None;

        if !gate_bypassed_by_approval {
            let gate_new_agents = config
                .map(|c| c.require_operator_approval_for_new_agents)
                .unwrap_or(true);
            let root_sid = run_context
                .and_then(|rc| Some(rc.root_session_id.clone()).filter(|s| !s.is_empty()))
                .or_else(|| {
                    session_id.map(|s| crate::runtime::content_store::root_session_id(s).to_string())
                });
            let mut capability_delta = check_capability_delta(
                &gateway_store,
                gateway_dir,
                &args.agent_id,
                &args.revision_id,
                &current_capabilities,
                delta_mode,
                gate_new_agents,
            )?;
            if capability_delta.is_some() {
                if let Some(ref root) = root_sid {
                    match crate::runtime::session_envelope::find_promote_with_envelope_id(
                        &gateway_store,
                        root,
                        &args.agent_id,
                        &current_capabilities,
                    ) {
                        Ok(Some(envelope_id)) => {
                            tracing::info!(
                                target: "promotion",
                                agent_id = %args.agent_id,
                                root_session_id = %root,
                                envelope_id,
                                "pre-authorized by session envelope PromoteWith (P-2.27)"
                            );
                            pre_auth_envelope_id = Some(envelope_id);
                            capability_delta = None;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::debug!(
                                target: "promotion",
                                error = %e,
                                "session envelope pre-auth check failed; falling through to capability gate"
                            );
                        }
                    }
                }
            }
            if let Some(delta) = capability_delta {
                let outgoing_revision_id = gateway_store
                    .resolve_alias(&args.agent_id)?
                    .map(|alias| alias.revision_id)
                    .unwrap_or_default();
                let added_capabilities: Vec<String> = delta.added.clone();
                let broadened_capabilities: Vec<String> = delta
                    .broadened
                    .iter()
                    .map(|b| b.capability_type.clone())
                    .collect();

                // Cross-workflow dedupe: if an identical RevisionPromote approval
                // already exists (pending or approved) for this agent/revision/delta,
                // reuse it instead of minting a new request ID. This prevents
                // retries across fork/resume workflows from flooding the operator
                // with duplicate capability-ack gates.
                if let Some(root) = run_context
                    .and_then(|rc| Some(rc.root_session_id.clone()).filter(|s| !s.is_empty()))
                    .or_else(|| {
                        session_id
                            .map(|s| crate::runtime::content_store::root_session_id(s).to_string())
                    })
                {
                    if let Some(existing) = gateway_store
                        .find_matching_revision_promote_approval_for_root(
                            &root,
                            &args.agent_id,
                            &args.revision_id,
                            &added_capabilities,
                            &broadened_capabilities,
                        )?
                    {
                        if let Some(guard) = single_flight_guard.as_mut() {
                            guard.disarm();
                        }
                        let request_id = existing.request_id.clone();
                        let payload = serde_json::json!({
                            "added": delta.added,
                            "broadened": delta.broadened,
                        });
                        return Ok(serde_json::json!({
                            "ok": existing.status
                                == Some(autonoetic_types::background::ApprovalStatus::Approved),
                            "error_type": "permission",
                            "error": "capability_delta_requires_approval",
                            "message": if outgoing_revision_id.is_empty() {
                                format!(
                                    "Promoting new agent '{}' for the first time: all declared capabilities require operator acknowledgement (R++2 / promotion-completeness). Operator approval is required.",
                                    args.agent_id
                                )
                            } else {
                                format!(
                                    "Capability set broadened relative to outgoing revision '{}'. Operator approval is required (R++2).",
                                    outgoing_revision_id
                                )
                            },
                            "approval_required": existing.status
                                != Some(autonoetic_types::background::ApprovalStatus::Approved),
                            "request_id": request_id,
                            "approval_ref": request_id,
                            "added_capabilities": added_capabilities,
                            "broadened_capabilities": broadened_capabilities,
                            "delta": payload,
                            "repair_hint": "Operator must approve the request and acknowledge each added/broadened capability by name. Then retry agent_revision_promote with `approval_ref`.",
                        })
                        .to_string());
                    }
                }

                let payload = serde_json::json!({
                    "added": delta.added,
                    "broadened": delta.broadened,
                });

                let request_id = format!(
                    "apr-{}",
                    &uuid::Uuid::new_v4().to_string().replace('-', "")[..16]
                );
                let action = autonoetic_types::background::ScheduledAction::RevisionPromote {
                    agent_id: args.agent_id.clone(),
                    revision_id: args.revision_id.clone(),
                    outgoing_revision_id: outgoing_revision_id.clone(),
                    added_capabilities: added_capabilities.clone(),
                    broadened_capabilities: broadened_capabilities.clone(),
                    payload: Some(payload.clone()),
                };
                let approval_level = config
                    .map(|cfg| crate::scheduler::approval::resolve_approval_level(cfg, &action))
                    .unwrap_or(autonoetic_types::background::ApprovalLevel::Operator);
                let (root_session_id, workflow_id, task_id) =
                    approval_execution_context(run_context, session_id);
                let mut req = autonoetic_types::background::ApprovalRequest {
                    request_id: request_id.clone(),
                    agent_id: manifest.agent.id.clone(),
                    session_id: session_id.unwrap_or("").to_string(),
                    root_session_id,
                    workflow_id,
                    task_id,
                    action,
                    created_at: chrono::Utc::now().to_rfc3339(),
                    status: None,
                    decided_at: None,
                    decided_by: None,
                    reason: args.reason.clone().or_else(|| {
                        Some(if outgoing_revision_id.is_empty() {
                            format!(
                                "First promotion of new agent '{}' (revision '{}') — operator must acknowledge all declared capabilities",
                                args.agent_id, args.revision_id
                            )
                        } else {
                            format!(
                                "Promote revision '{}' would broaden capabilities relative to '{}'",
                                args.revision_id, outgoing_revision_id
                            )
                        })
                    }),
                    evidence_ref: None,
                    decision_reason: None,
                    approval_level: approval_level.clone(),
                    min_dwell_ms: None,
                    confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
                };

                gateway_store.create_approval(&mut req)?;
                if let (Some(config), Some((workflow_id, spec))) = (config, single_flight_scope.as_ref()) {
                    crate::scheduler::single_flight::attach_approval_request(
                        config,
                        workflow_id,
                        &spec.dedupe_key,
                        &request_id,
                    )?;
                }
                if let Some(guard) = single_flight_guard.as_mut() {
                    guard.disarm();
                }

                return Ok(serde_json::json!({
                    "ok": false,
                    "error_type": "permission",
                    "error": "capability_delta_requires_approval",
                    "message": if outgoing_revision_id.is_empty() {
                        format!(
                            "Promoting new agent '{}' for the first time: all declared capabilities require operator acknowledgement (R++2 / promotion-completeness). Operator approval is required.",
                            args.agent_id
                        )
                    } else {
                        format!(
                            "Capability set broadened relative to outgoing revision '{}'. Operator approval is required (R++2).",
                            outgoing_revision_id
                        )
                    },
                    "approval_required": true,
                    "request_id": request_id,
                    "approval_ref": request_id,
                    "added_capabilities": added_capabilities,
                    "broadened_capabilities": broadened_capabilities,
                    "delta": payload,
                    "repair_hint": "Operator must approve the request and acknowledge each added/broadened capability by name. Then retry agent_revision_promote with `approval_ref`.",
                })
                .to_string());
            }
        }

        // Deserialize capabilities from the frontmatter using the same lenient pipeline
        // used by create_from_intent, so shorthand strings and field normalization are handled.
        let (needs_artifact_gate, has_high_risk) = {
            let mut artifact_required = false;
            let mut high_risk = false;
            for cap in &current_capabilities {
                if crate::runtime::install_contract::requires_artifact_review(cap) {
                    artifact_required = true;
                }
                if crate::runtime::install_contract::is_high_risk_capability(cap) {
                    high_risk = true;
                }
            }
            (artifact_required, high_risk)
        };

        // Promotion gate mode — derived from the revision's capabilities and
        // artifact shape, **not** from anything the orchestrator declares.
        // See docs/archived/sealed-network-evaluation-plan.md §3.5.5.
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        enum PromotionGateMode {
            /// CodeExecution/AgentSpawn, or NetworkAccess + artifact:
            /// auditor PASS + evaluator PASS, distinct identities.
            Full,
            /// Intent-only artifact (pure-reasoning agent) without
            /// high-risk capabilities: auditor PASS, auditor identity
            /// distinct from revision proposer. Evaluator skipped — the
            /// behavioural-evaluation mechanism for pure-skill agents
            /// is not yet implemented.
            AuditOnly,
            /// Federation jury mode: operator approval required because
            /// federation roles (static_evaluator, unit_test_runner,
            /// sealed_evaluator) recorded verdicts for this artifact.
            FullJury,
        }

        // Returns Ok(None) when the gate passes, Ok(Some(json)) carrying a
        // structured P-5.11 failure envelope (ok:false + stable `error` code)
        // when blocked, and Err only for genuine infrastructure failures.
        // Messages are kept verbatim (callers/tests read them); the stable code
        // lets an orchestrator branch on one field instead of parsing prose.
        let enforce_promotion_gate = |artifact_id: &str,
                                      mode: PromotionGateMode,
                                      missing_record_message: &str|
         -> anyhow::Result<Option<String>> {
            use autonoetic_types::tool_error::ToolError;
            let promo_store = crate::runtime::promotion_store::PromotionStore::new(gateway_dir)?;
            let _ = promo_store.bind_content_digest_if_unset(artifact_id, &rev.content_digest)?;
            let Some(record) = promo_store.get_promotion(artifact_id) else {
                return Ok(Some(
                    ToolError::permission(missing_record_message.to_string())
                        .with_code("promotion_record_missing")
                        .with_repair_hint(
                            "Obtain the required gate pass record(s) for this artifact, then retry agent_revision_promote.",
                        )
                        .to_error_response(),
                ));
            };

            let record_content_digest = record.content_digest.as_deref().unwrap_or("<none>");
            if record.content_digest.as_deref() != Some(rev.content_digest.as_str()) {
                return Ok(Some(
                    ToolError::permission(format!(
                        "Promotion gate: promotion record for artifact '{}' is bound to content digest '{}' \
                         but revision requires '{}'. Re-run gate roles for this revision content.",
                        artifact_id, record_content_digest, rev.content_digest
                    ))
                    .with_code("promotion_gate_content_digest_mismatch")
                    .with_repair_hint("Re-run the gate roles against this revision's content, then retry.")
                    .to_error_response(),
                ));
            }

            if !record.auditor_pass {
                return Ok(Some(
                    ToolError::permission(format!(
                        "Promotion gate: auditor did not pass for artifact '{}'. \
                     Fix the audit findings and re-run auditor.default.",
                        artifact_id
                    ))
                    .with_code("auditor_pass_missing")
                    .with_repair_hint("Obtain a passing auditor record for this artifact, then retry.")
                    .to_error_response(),
                ));
            }
            let Some(audit_id) = record.auditor_id.as_deref() else {
                return Ok(Some(
                    ToolError::permission(format!(
                        "Promotion gate: auditor identity missing for artifact '{}' (P-2.17). \
                     Re-run auditor.default to record its identity.",
                        artifact_id
                    ))
                    .with_code("auditor_identity_missing")
                    .with_repair_hint("Re-record the audit with an attributed identity, then retry.")
                    .with_enforced_rules(vec!["P-2.17".to_string()])
                    .to_error_response(),
                ));
            };

            match mode {
                PromotionGateMode::Full => {
                    if !(record.evaluator_pass
                        || record.sealed_evaluator_pass
                        || record.static_evaluator_pass)
                    {
                        return Ok(Some(
                            ToolError::permission(format!(
                                "Promotion gate: no evaluator role passed for artifact '{}'. \
                         Fix the evaluation findings and re-run sealed_evaluator.default or static_evaluator.default.",
                                artifact_id
                            ))
                            .with_code("evaluator_pass_missing")
                            .with_repair_hint("Obtain a passing evaluator record (evaluator/sealed/static), then retry.")
                            .to_error_response(),
                        ));
                    }
                    let Some(eval_id) = record
                        .evaluator_id
                        .as_deref()
                        .or(record.sealed_evaluator_id.as_deref())
                        .or(record.static_evaluator_id.as_deref())
                    else {
                        return Ok(Some(
                            ToolError::permission(format!(
                                "Promotion gate: evaluator identity missing for artifact '{}' (P-2.17). \
                                 Re-run an evaluator role to record its identity.",
                                artifact_id
                            ))
                            .with_code("evaluator_identity_missing")
                            .with_repair_hint("Re-record the evaluation with an attributed identity, then retry.")
                            .with_enforced_rules(vec!["P-2.17".to_string()])
                            .to_error_response(),
                        ));
                    };
                    if eval_id == audit_id {
                        return Ok(Some(
                            ToolError::permission(format!(
                                "Promotion gate: evaluator and auditor are the same agent '{}' (P-2.17). \
                         A single agent cannot self-approve. Use distinct evaluator and auditor agents.",
                                eval_id
                            ))
                            .with_code("gate_identity_collision")
                            .with_repair_hint("Use distinct evaluator and auditor identities, then retry.")
                            .with_enforced_rules(vec!["P-2.17".to_string()])
                            .to_error_response(),
                        ));
                    }
                }
                PromotionGateMode::AuditOnly => {
                    // P-2.17 reduced: auditor must be a distinct identity
                    // from the agent that proposed the install. With no
                    // evaluator in this mode, the proposer is the relevant
                    // counterparty for the self-approval ban.
                    if audit_id == rev.created_by_id {
                        return Ok(Some(
                            ToolError::permission(format!(
                                "Promotion gate: auditor '{}' is the same identity that proposed revision '{}' (P-2.17, audit-only). \
                         A single agent cannot propose and audit. Use a distinct auditor identity.",
                                audit_id, args.revision_id
                            ))
                            .with_code("gate_identity_collision")
                            .with_repair_hint("Use an auditor identity distinct from the proposer, then retry.")
                            .with_enforced_rules(vec!["P-2.17".to_string()])
                            .to_error_response(),
                        ));
                    }
                }
                // FullJury enforcement is handled separately after the
                // legacy gate; this variant is only used for event emission.
                PromotionGateMode::FullJury => {}
            }

            // P-2.26: All executed gate roles must pass.
            if record.unit_test_runner_id.is_some() && !record.unit_test_runner_pass {
                return Ok(Some(
                    ToolError::permission(format!(
                        "Promotion gate: unit_test_runner did not pass for artifact '{}' (P-2.26). \
                     Fix the failing tests and re-run unit_test_runner.default.",
                        artifact_id
                    ))
                    .with_code("unit_test_runner_pass_missing")
                    .with_repair_hint("Resolve the failing tests and re-record a unit_test_runner pass, then retry.")
                    .with_enforced_rules(vec!["P-2.26".to_string()])
                    .to_error_response(),
                ));
            }

            let has_unresolved = rev
                .metadata_json
                .get("has_unresolved_dependencies")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if has_unresolved {
                return Ok(Some(
                    ToolError::permission(
                        "Promotion gate: revision has unresolved dependencies. \
                     Run packager.default to install dependencies as layers, \
                     then re-submit the revision."
                            .to_string(),
                    )
                    .with_code("unresolved_dependencies")
                    .with_repair_hint("Resolve the revision's dependencies (install as layers), then re-submit and retry.")
                    .to_error_response(),
                ));
            }
            Ok(None)
        };

        // Emit a causal event after each enforced promotion gate so forensics
        // can confirm which mode was applied to a given revision.
        let emit_gate_event = |mode: PromotionGateMode, artifact_id: Option<&str>| {
            let event = autonoetic_types::causal_chain::CausalEventRecord {
                event_id: format!("promote-gate-{}", uuid::Uuid::new_v4()),
                agent_id: manifest.agent.id.clone(),
                session_id: session_id.unwrap_or("").to_string(),
                turn_id: None,
                event_seq: 0,
                timestamp: chrono::Utc::now().to_rfc3339(),
                category: "revision".to_string(),
                action: "revision.promotion_gate_enforced".to_string(),
                status: "active".to_string(),
                enforced_rules: vec!["P-2.8".to_string(), "P-2.17".to_string(), "P-2.22".to_string(), "P-2.26".to_string()],
                target: artifact_id.map(|s| s.to_string()),
                payload: Some(
                    serde_json::json!({
                        "revision_id": &args.revision_id,
                        "agent_id": &args.agent_id,
                        "artifact_id": artifact_id,
                        "mode": match mode {
                            PromotionGateMode::Full => "full",
                            PromotionGateMode::AuditOnly => "audit_only",
                            PromotionGateMode::FullJury => "full_jury",
                        },
                    })
                    .to_string(),
                ),
                payload_ref: None,
                evidence_ref: None,
                reason: None,
            };
            let _ = gateway_store.create_causal_event(&event);
        };

        if needs_artifact_gate {
            // CodeExecution or AgentSpawn: must have artifact + full eval/audit gate.
            let artifact_id = rev.artifact_id.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Promotion gate: revision '{}' has CodeExecution/AgentSpawn but no artifact_id. \
                     Agents with code execution or agent spawning require a reviewed artifact.",
                    args.revision_id
                )
            })?;
            if let Some(json) = enforce_promotion_gate(
                artifact_id,
                PromotionGateMode::Full,
                &format!(
                    "Promotion gate: no promotion.record found for artifact '{}'. \
                     Agents with CodeExecution/AgentSpawn require both \
                     evaluator and auditor pass records before promotion.",
                    artifact_id
                ),
            )? {
                return Ok(json);
            }
            emit_gate_event(PromotionGateMode::Full, Some(artifact_id));
        } else if has_high_risk && rev.artifact_id.is_some() {
            // NetworkAccess without CodeExecution/AgentSpawn, but an artifact was provided.
            // Still apply the full eval+audit gate since code exists to review.
            let artifact_id = rev.artifact_id.as_deref().unwrap();
            if let Some(json) = enforce_promotion_gate(
                artifact_id,
                PromotionGateMode::Full,
                &format!(
                    "Promotion gate: no promotion.record found for artifact '{}'. \
                     Agents with NetworkAccess and a code artifact require both \
                     evaluator and auditor pass records before promotion.",
                    artifact_id
                ),
            )? {
                return Ok(json);
            }
            emit_gate_event(PromotionGateMode::Full, Some(artifact_id));
        } else if rev.artifact_id.is_some() && !current_capabilities.is_empty() {
            // Pure-skill intent-only bundle (no CodeExecution/AgentSpawn, no
            // high-risk capabilities) that declares non-empty capabilities
            // and ships an artifact. Audit_only mode — auditor pass required;
            // evaluator skipped pending the behavioural-evaluation mechanism
            // for pure-skill agents. See sealed-network RFC §3.5.5.
            //
            // The `!current_capabilities.is_empty()` guard preserves the
            // existing direct-promote path for zero-capability sandboxed
            // scripts: an agent that declares no capabilities cannot
            // mutate gateway state via tool calls, and the audit surface
            // for such an agent is trivial. Runtime capability enforcement
            // on every tool call remains the security gate for that case.
            let artifact_id = rev.artifact_id.as_deref().unwrap();
            if let Some(json) = enforce_promotion_gate(
                artifact_id,
                PromotionGateMode::AuditOnly,
                &format!(
                    "Promotion gate: no auditor promotion.record found for artifact '{}'. \
                     Pure-skill agents (reasoning-only with declared capabilities and an \
                     intent-only artifact bundle) require an auditor pass record before \
                     promotion.",
                    artifact_id
                ),
            )? {
                return Ok(json);
            }
            emit_gate_event(PromotionGateMode::AuditOnly, Some(artifact_id));
        } else {
            // Reaching here ⇒ either zero declared capabilities, or a
            // capability-bearing revision with no artifact (artifact-bearing
            // capability agents were gated by the branches above). Fail closed:
            // a capability-bearing revision with nothing to review must NOT
            // direct-promote (the previous fall-through was the fail-open). Only
            // the inoffensive zero-capability class is relieved — it cannot
            // invoke any privileged tool, so runtime capability enforcement
            // bounds its blast radius — and only while the cursor allows it
            // (promotion-completeness invariant, docs/design/).
            let inoffensive = current_capabilities.is_empty();
            let cursor_allows = config
                .map(|c| c.allow_zero_capability_direct_promote)
                .unwrap_or(true);
            if !(inoffensive && cursor_allows) {
                let (message, repair_hint) = if !inoffensive {
                    (
                        format!(
                            "Promotion gate: revision '{}' declares capabilities but ships no reviewable \
                             artifact. A capability-bearing agent cannot be promoted without an artifact \
                             bundle and the required audit records (fail-closed).",
                            args.revision_id
                        ),
                        "Attach a reviewed artifact bundle and obtain the required auditor (and, for \
                         high-risk capabilities, evaluator) pass records, then retry agent_revision_promote.",
                    )
                } else {
                    (
                        format!(
                            "Promotion gate: zero-capability direct-promote is disabled \
                             (allow_zero_capability_direct_promote=false); revision '{}' must pass the full \
                             review gate.",
                            args.revision_id
                        ),
                        "Provide a reviewed artifact with an auditor pass record, or re-enable \
                         allow_zero_capability_direct_promote, then retry agent_revision_promote.",
                    )
                };
                return Ok(serde_json::json!({
                    "ok": false,
                    "error_type": "permission",
                    "error": "promotion_incomplete",
                    "message": message,
                    "repair_hint": repair_hint,
                })
                .to_string());
            }
            // inoffensive (zero-capability) + cursor on → direct promote.
        }

        // FullJury gate: when federation roles have recorded verdicts, require
        // operator approval via an approved escalation. This is a fifth branch
        // that activates on top of the legacy gate: if federation verdicts exist,
        // an approved escalation must also exist. If no federation verdicts, the
        // legacy check above is sufficient.
        if let Some(artifact_id) = rev.artifact_id.as_deref() {
            let promo_store =
                crate::runtime::promotion_store::PromotionStore::new(gateway_dir)?;
            if promo_store.has_federation_roles(artifact_id) {
                let fed_ids = promo_store.federation_agent_ids(artifact_id);

                // P-2.17 extended: each federation role identity must differ
                // from every other federation role identity AND from the
                // revision proposer.
                let proposer = rev.created_by_id.as_str();
                for id in &fed_ids {
                    if id == proposer {
                        return Ok(autonoetic_types::tool_error::ToolError::permission(format!(
                            "Promotion gate (FullJury): federation role '{}' is the same identity \
                         that proposed revision '{}' (P-2.17). Each federation role must be \
                         a distinct agent from the proposer.",
                            id, args.revision_id
                        ))
                        .with_code("jury_identity_collision")
                        .with_repair_hint("Use federation role identities distinct from the proposer, then retry.")
                        .with_enforced_rules(vec!["P-2.17".to_string(), "P-2.22".to_string()])
                        .to_error_response());
                    }
                }
                if fed_ids.len() > 1 {
                    for i in 0..fed_ids.len() {
                        for j in (i + 1)..fed_ids.len() {
                            if fed_ids[i] == fed_ids[j] {
                                return Ok(autonoetic_types::tool_error::ToolError::permission(format!(
                                    "Promotion gate (FullJury): federation roles '{}' and '{}' \
                                 are the same agent (P-2.17). Each federation role must be \
                                 a distinct agent.",
                                    fed_ids[i], fed_ids[j]
                                ))
                                .with_code("jury_identity_collision")
                                .with_repair_hint("Use distinct identities for each federation role, then retry.")
                                .with_enforced_rules(vec!["P-2.17".to_string(), "P-2.22".to_string()])
                                .to_error_response());
                            }
                        }
                    }
                }

                let escalation = gateway_store.find_escalation(
                    artifact_id,
                    &args.revision_id,
                    autonoetic_types::escalation::EscalationStatus::Approved,
                )?;
                let escalation = match escalation {
                    Some(e) => Some(e),
                    None => gateway_store.find_approved_escalation_for_artifact(artifact_id)?,
                };
                if escalation.is_none() {
                    return Ok(autonoetic_types::tool_error::ToolError::permission(format!(
                        "Promotion gate (FullJury): revision '{}' has federation role verdicts \
                     but no approved operator escalation. \
                     The planner must call federation_escalate with revision_id='{}' \
                     and the operator must approve before promotion.",
                        args.revision_id, args.revision_id
                    ))
                    .with_code("jury_escalation_required")
                    .with_repair_hint("Obtain an approved operator escalation for this revision, then retry.")
                    .with_enforced_rules(vec!["P-2.22".to_string()])
                    .to_error_response());
                }

                emit_gate_event(PromotionGateMode::FullJury, Some(artifact_id));
            }
        }

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

        // Protected-agent promotion gate (issue #21).
        // Critical agents (e.g. agent-factory.default) cannot be promoted
        // without eval evidence. This closes the recursive-trust loop: a
        // regressed agent-factory is exactly the agent that cannot be trusted
        // to fix itself without independent verification.
        if let Some(cfg) = config {
            if cfg.protected_agents.enabled
                && cfg
                    .protected_agents
                    .agents
                    .iter()
                    .any(|a| a == &args.agent_id)
            {
                if args.required_eval_run_id.is_none() {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "error_type": "permission",
                        "error": "protected_agent_requires_eval_run",
                        "message": format!(
                            "Agent '{}' is protected (issue #21). Promotion requires a passed eval run as evidence. \
                             Provide `required_eval_run_id` referencing a successful eval run for this revision.",
                            &args.agent_id
                        ),
                        "protected_agent": &args.agent_id,
                        "repair_hint": "Run an eval suite against this revision, then retry promotion with `required_eval_run_id` pointing to the passed run.",
                    })
                    .to_string());
                }
            }
        }

        // Promotion safety governor (issue #25): velocity / flapping /
        // eval-regression gates. Bypassed when `force = true` (and a
        // `force_reason` is recorded for the audit trail). Sits between the
        // protected-agent gate and the sentinel sweep so a rejection here
        // avoids the cost of a sentinel pre-promotion run.
        if let Some(cfg) = config {
            let force_requested = args.force.unwrap_or(false);
            if force_requested {
                let reason = args.force_reason.as_deref().unwrap_or("").trim();
                if reason.is_empty() {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "error_type": "validation",
                        "error": "governor_force_requires_reason",
                        "message": "Passing `force: true` to bypass the promotion safety governor requires a non-empty `force_reason`.",
                        "repair_hint": "Supply `force_reason` describing why the governor is being overridden (e.g. 'planned rollback re-run after operator review').",
                    })
                    .to_string());
                }
                if reason.len() > 512 {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "error_type": "validation",
                        "error": "governor_force_reason_too_long",
                        "message": format!("`force_reason` must be at most 512 characters (got {}).", reason.len()),
                        "repair_hint": "Shorten the override justification.",
                    })
                    .to_string());
                }
                if cfg.promotion_governor.enabled {
                    crate::runtime::promotion_governor::emit_override_event(
                        gateway_store.as_ref(),
                        &manifest.agent.id,
                        session_id,
                        &args.agent_id,
                        &args.revision_id,
                        reason,
                    );
                }
            } else if cfg.promotion_governor.enabled {
                if let Some(rejection) = crate::runtime::promotion_governor::run_governor_checks(
                    &cfg.promotion_governor,
                    gateway_store.as_ref(),
                    gateway_dir,
                    &args.agent_id,
                    &args.revision_id,
                )? {
                    crate::runtime::promotion_governor::emit_rejected_event(
                        gateway_store.as_ref(),
                        &manifest.agent.id,
                        session_id,
                        &args.agent_id,
                        &args.revision_id,
                        &rejection,
                    );
                    return Ok(rejection.to_tool_error().to_string());
                }
            }
        }

        // Security sentinel pre-promotion gate (fail-closed).
        // Runs a Phase-1 sweep scoped to the agent being promoted — only
        // critical findings attributable to this agent block its promotion
        // (issue #155). Findings against other agents do not interfere.
        if let Some(cfg) = config {
            if cfg.sentinel.enabled && cfg.sentinel.promotion_gate_enabled {
                match crate::sentinel::check_pre_promotion(
                    Arc::clone(&gateway_store),
                    &cfg.sentinel.sentinel_revision_id,
                    &args.agent_id,
                    cfg.sentinel.promotion_gate_timeout_secs,
                ) {
                    Ok(crate::sentinel::GateOutcome::Passed) => {
                        tracing::debug!(
                            target: "sentinel.promotion_gate",
                            agent_id = %args.agent_id,
                            revision_id = %args.revision_id,
                            "Sentinel pre-promotion gate passed"
                        );
                    }
                    Ok(crate::sentinel::GateOutcome::Blocked { reason, critical_count }) => {
                        return Ok(serde_json::json!({
                            "ok": false,
                            "error_type": "sentinel_gate",
                            "error": "sentinel_critical_findings_block_promotion",
                            "message": format!(
                                "Sentinel pre-promotion gate blocked: {} critical finding(s). Resolve findings before promoting.",
                                critical_count
                            ),
                            "critical_count": critical_count,
                            "reason": reason,
                            "repair_hint": "Review findings in the security_findings table, resolve or triage them, then retry promotion.",
                        })
                        .to_string());
                    }
                    Err(e) => {
                        // Fail-closed: timeout or sweep error blocks promotion.
                        return Ok(serde_json::json!({
                            "ok": false,
                            "error_type": "sentinel_gate",
                            "error": "sentinel_gate_failed",
                            "message": format!("Sentinel pre-promotion gate failed (fail-closed): {}", e),
                            "repair_hint": "Check gateway logs for sentinel errors. If sentinel is misconfigured, disable promotion_gate_enabled in config.",
                        })
                        .to_string());
                    }
                }
            }
        }

        // Smoke-test gate for new agent installation.
        // When configured as `required`, a brand-new agent (no existing alias) must
        // have been executed at least once via agent_spawn with revision_id.
        let smoke_test_required = config
            .map(|c| c.agent_install_smoke_test)
            .unwrap_or_default()
            == autonoetic_types::config::AgentInstallSmokeTestMode::Required;
        let is_new_agent = gateway_store.resolve_alias(&args.agent_id)?.is_none();
        if smoke_test_required && is_new_agent {
            let Some(workflow_id) = args.smoke_test_workflow_id.as_deref() else {
                return Ok(serde_json::json!({
                    "ok": false,
                    "error_type": "validation",
                    "error": "smoke_test_required",
                    "message": format!(
                        "Agent '{}' is new and agent_install_smoke_test is 'required'. \
                         Provide smoke_test_workflow_id and smoke_test_task_id from a successful smoke-test run.",
                        args.agent_id
                    ),
                    "repair_hint": "Run the candidate revision once with agent_spawn(revision_id=...), wait for success, then retry promotion with smoke_test_workflow_id and smoke_test_task_id.",
                })
                .to_string());
            };
            let Some(task_id) = args.smoke_test_task_id.as_deref() else {
                return Ok(serde_json::json!({
                    "ok": false,
                    "error_type": "validation",
                    "error": "smoke_test_required",
                    "message": format!(
                        "Agent '{}' is new and agent_install_smoke_test is 'required'. \
                         Provide smoke_test_task_id from a successful smoke-test run.",
                        args.agent_id
                    ),
                    "repair_hint": "Run the candidate revision once with agent_spawn(revision_id=...), wait for success, then retry promotion with smoke_test_workflow_id and smoke_test_task_id.",
                })
                .to_string());
            };
            if let Err(e) = verify_smoke_test_task(
                config.unwrap(),
                &gateway_store,
                workflow_id,
                task_id,
                &args.agent_id,
                &args.revision_id,
            ) {
                return Ok(serde_json::json!({
                    "ok": false,
                    "error_type": "validation",
                    "error": "smoke_test_failed_or_mismatched",
                    "message": format!(
                        "Smoke-test evidence for new agent '{}' is missing or invalid: {}",
                        args.agent_id,
                        e
                    ),
                    "repair_hint": "Run the candidate revision once with agent_spawn(revision_id=...) and confirm the task succeeds, then retry promotion with the correct workflow_id and task_id.",
                })
                .to_string());
            }
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

        let pre_authorization = pre_auth_envelope_id.map(|eid| {
            serde_json::json!({
                "method": "envelope",
                "envelope_id": eid,
                "rule": "P-2.27",
            }).to_string()
        });
        let previous_revision_id = gateway_store.atomic_promote(
            &args.agent_id,
            &args.revision_id,
            &promotion_id,
            "agent",
            &manifest.agent.id,
            args.reason.as_deref(),
            args.required_eval_run_id.as_deref(),
            pre_authorization.as_deref(),
        )?;

        crate::bootstrap::update_latest_symlink(gateway_dir, &args.agent_id, &args.revision_id);

        if let Some(art_id) = &rev.artifact_id {
            if let Err(e) = gateway_store.promote_artifact_ref_to_global(art_id) {
                tracing::warn!(
                    target: "agent_revision",
                    artifact_id = %art_id,
                    "Failed to promote artifact ref to global scope: {}", e
                );
            }
        }

        let short_ref = format!("{}@rev_{}", args.agent_id, rev.short_id);
        let mut response = serde_json::json!({
            "ok": true,
            "status": "promoted",
            "agent_id": args.agent_id,
            "revision_id": args.revision_id,
            "short_ref": short_ref,
            "previous_revision_id": previous_revision_id,
            "promotion_id": promotion_id,
            // Terminal signal: promotion IS installation — the alias now points
            // at this revision. Make the single next action explicit so the
            // orchestrator spawns the agent instead of rebuilding/re-promoting.
            "installed": true,
            "next": format!(
                "Agent '{}' is now the active (installed) revision. To use it, call \
                 agent_spawn with agent_id '{}'. Do not rebuild or promote it again.",
                args.agent_id, args.agent_id
            ),
        });
        if let Some(eid) = pre_auth_envelope_id {
            response["pre_authorized_by_envelope"] = serde_json::json!(eid);
            response["pre_auth_rule"] = serde_json::json!("P-2.27");
        }
        Ok(response.to_string())
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
        "agent_revision_rollback"
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
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: RevisionRollbackArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments: {}", e))?;

        crate::runtime::tools::validate_agent_id(&args.agent_id)?;
        let decision = policy.can_agent_revision(&args.agent_id);
        if !decision.is_allowed() {
            return Err(tagged::Tagged::permission_with_rules(
                anyhow::anyhow!(
                    "Permission Denied: missing AgentRevision capability for '{}'",
                    args.agent_id
                ),
                decision
                    .enforced_rules
                    .into_iter()
                    .map(|rule| rule.to_string())
                    .collect(),
            )
            .into());
        }

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

        let single_flight_scope = if let (Some(config), Some(session_id)) = (config, session_id) {
            let root_session_id = crate::runtime::content_store::root_session_id(session_id);
            let workflow = crate::scheduler::ensure_workflow_for_root_session(
                config,
                Some(gateway_store.as_ref()),
                &root_session_id,
                Some(manifest.agent.id.as_str()),
            )?;
            let spec = crate::scheduler::single_flight::build_spec(
                &workflow.workflow_id,
                "rollback",
                &args.agent_id,
                rev.source_ref.clone().or_else(|| Some(target_revision_id.clone())),
                None,
            );
            match crate::scheduler::single_flight::try_acquire_reservation(
                config,
                Some(gateway_store.as_ref()),
                &spec,
                None,
                None,
            )? {
                crate::scheduler::single_flight::AcquireOutcome::Acquired(_) => {
                    Some((workflow.workflow_id, spec))
                }
                crate::scheduler::single_flight::AcquireOutcome::Coalesced(existing) => {
                    crate::scheduler::append_workflow_event(
                        config,
                        Some(gateway_store.as_ref()),
                        &autonoetic_types::workflow::WorkflowEventRecord {
                            event_id: format!("wevt-{}", &uuid::Uuid::new_v4().to_string()[..8]),
                            workflow_id: workflow.workflow_id.clone(),
                            task_id: existing.existing_task_id.clone(),
                            event_type: "workflow.single_flight.coalesced".to_string(),
                            agent_id: Some(args.agent_id.clone()),
                            payload: serde_json::json!({
                                "status": "coalesced",
                                "stage_kind": existing.stage_kind,
                                "dedupe_key": existing.dedupe_key,
                                "existing_task_id": existing.existing_task_id,
                                "approval_request_id": existing.approval_request_id,
                                "retry_advice": "wait",
                            }),
                            occurred_at: chrono::Utc::now().to_rfc3339(),
                        },
                    )?;
                    return Ok(serde_json::json!({
                        "ok": true,
                        "status": "coalesced",
                        "existing_task_id": existing.existing_task_id,
                        "approval_ref": existing.approval_request_id,
                        "dedupe_key": existing.dedupe_key,
                        "retry_advice": "wait",
                        "message": "Equivalent durable rollback operation is already active. Wait for the existing operation instead."
                    })
                    .to_string());
                }
            }
        } else {
            None
        };
        let _single_flight_guard = single_flight_scope
            .as_ref()
            .and_then(|(workflow_id, spec)| {
                config.map(|config| {
                    SingleFlightReleaseGuard::new(
                        config,
                        workflow_id.clone(),
                        spec.dedupe_key.clone(),
                    )
                })
            });

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
        "agent_revision_diff"
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
        let from_decision = policy.can_agent_revision(&from_ref.agent_id);
        let to_decision = policy.can_agent_revision(&to_ref.agent_id);
        if !from_decision.is_allowed() || !to_decision.is_allowed() {
            let mut enforced_rules: Vec<String> = from_decision
                .enforced_rules
                .into_iter()
                .map(|rule| rule.to_string())
                .collect();
            for rule in to_decision
                .enforced_rules
                .into_iter()
                .map(|rule| rule.to_string())
            {
                if !enforced_rules.contains(&rule) {
                    enforced_rules.push(rule);
                }
            }
            return Err(tagged::Tagged::permission_with_rules(
                anyhow::anyhow!(
                    "Permission Denied: agent '{}' lacks AgentRevision capability for requested targets",
                    manifest.agent.id
                ),
                enforced_rules,
            )
            .into());
        }

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

/// Resolve `(root_session_id, workflow_id, task_id)` for approval rows created
/// from native tools. Mirrors `human_gate::resolve_execution_context` so
/// workflow-bound promote gates unblock via `unblock_task_on_approval`.
fn approval_execution_context(
    run_context: Option<&NativeToolRunContext>,
    session_id: Option<&str>,
) -> (Option<String>, Option<String>, Option<String>) {
    if let Some(rc) = run_context {
        let root_session_id = if rc.root_session_id.is_empty() {
            None
        } else {
            Some(rc.root_session_id.clone())
        };
        (root_session_id, rc.workflow_id.clone(), rc.task_id.clone())
    } else if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
        (
            Some(crate::runtime::content_store::root_session_id(sid).to_string()),
            None,
            None,
        )
    } else {
        (None, None, None)
    }
}

/// Look up an existing approval by ID and decide whether the R++2 gate may be
/// bypassed for this retry. All four conditions must hold:
///   (a) the action is `RevisionPromote` for exactly this `(agent_id, revision_id)`,
///   (b) the approval status is `Approved`,
///   (c) the recorded `outgoing_revision_id` still matches the current alias —
///       i.e. the baseline the operator acknowledged against has not moved;
///       if it has, a fresh promote attempt must produce a new approval
///       against the new baseline (otherwise an unrelated revision flip
///       between approval-mint and retry could let unacknowledged caps through).
/// Verify that a smoke-test task ran the candidate revision successfully.
///
/// Returns `Ok(())` if the task exists, reached `Succeeded` status, and its
/// metadata records `_autonoetic_spawn_revision_id` matching the target revision.
fn verify_smoke_test_task(
    config: &autonoetic_types::config::GatewayConfig,
    gateway_store: &crate::scheduler::gateway_store::GatewayStore,
    workflow_id: &str,
    task_id: &str,
    agent_id: &str,
    revision_id: &str,
) -> anyhow::Result<()> {
    let task = crate::scheduler::workflow_store::load_task_run(config, Some(gateway_store), workflow_id, task_id)?
        .ok_or_else(|| anyhow::anyhow!("Smoke-test task '{}' not found in workflow '{}'", task_id, workflow_id))?;

    if task.agent_id != agent_id {
        anyhow::bail!(
            "Smoke-test task '{}' targeted agent '{}', not '{}'",
            task_id,
            task.agent_id,
            agent_id
        );
    }

    if !matches!(
        task.status,
        autonoetic_types::workflow::TaskRunStatus::Succeeded
    ) {
        anyhow::bail!(
            "Smoke-test task '{}' did not succeed (status: {:?}). The candidate revision cannot be promoted.",
            task_id,
            task.status
        );
    }

    let spawned_revision_id = task
        .metadata
        .as_ref()
        .and_then(|m| m.get("_autonoetic_spawn_revision_id"))
        .and_then(|v| v.as_str());

    match spawned_revision_id {
        Some(rid) if rid == revision_id => Ok(()),
        Some(rid) => anyhow::bail!(
            "Smoke-test task '{}' ran revision '{}', not '{}'",
            task_id,
            rid,
            revision_id
        ),
        None => anyhow::bail!(
            "Smoke-test task '{}' is not tagged with a candidate revision id",
            task_id
        ),
    }
}

fn check_revision_promote_approval(
    gateway_store: &crate::scheduler::gateway_store::GatewayStore,
    approval_ref: &str,
    agent_id: &str,
    revision_id: &str,
) -> anyhow::Result<bool> {
    let Some(req) = gateway_store.get_approval(approval_ref)? else {
        return Ok(false);
    };
    let autonoetic_types::background::ScheduledAction::RevisionPromote {
        agent_id: a_id,
        revision_id: r_id,
        outgoing_revision_id: approved_outgoing,
        ..
    } = &req.action
    else {
        return Ok(false);
    };
    if a_id != agent_id || r_id != revision_id {
        return Ok(false);
    }
    if !matches!(
        req.status,
        Some(autonoetic_types::background::ApprovalStatus::Approved)
    ) {
        return Ok(false);
    }
    // Baseline consistency: the alias must still point to the revision the
    // operator was acknowledging against when they approved.
    let current_alias = gateway_store.resolve_alias(agent_id)?;
    let current_outgoing = current_alias
        .as_ref()
        .map(|a| a.revision_id.as_str())
        .unwrap_or("");
    Ok(current_outgoing == approved_outgoing.as_str())
}

fn check_capability_delta(
    gateway_store: &crate::scheduler::gateway_store::GatewayStore,
    gateway_dir: &Path,
    agent_id: &str,
    revision_id: &str,
    current_capabilities: &[Capability],
    mode: CapabilityDeltaGateMode,
    gate_new_agents: bool,
) -> anyhow::Result<Option<autonoetic_types::capability::CapabilityDelta>> {
    if matches!(mode, CapabilityDeltaGateMode::Bootstrap) {
        return Ok(None);
    }

    // Determine the outgoing capability baseline. A brand-new agent (no current
    // alias) has an EMPTY baseline — every declared capability is "added"
    // relative to nothing — so a capability-bearing first promotion is maximal
    // broadening and requires operator approval (R++2 / the promotion-
    // completeness invariant). A zero-capability new agent yields an empty delta
    // and is relieved. Returning `None` here (the previous behavior) was the
    // fail-open that let new agents promote with no operator approval.
    let outgoing_capabilities = match gateway_store.resolve_alias(agent_id)? {
        Some(alias) => {
            if alias.revision_id == revision_id {
                return Ok(None); // already-active revision; nothing to compare
            }
            let outgoing_revision_dir = gateway_dir
                .join("revisions/agents")
                .join(agent_id)
                .join(&alias.revision_id);
            let outgoing_skill_path = outgoing_revision_dir.join("SKILL.md");
            let outgoing_skill_bytes = std::fs::read(&outgoing_skill_path).map_err(|e| {
                anyhow::anyhow!(
                    "Cannot read SKILL.md for outgoing revision '{}': {}",
                    alias.revision_id,
                    e
                )
            })?;
            let outgoing_skill_text = String::from_utf8_lossy(&outgoing_skill_bytes);
            let outgoing_frontmatter = crate::runtime::install_contract::extract_frontmatter_raw(
                &outgoing_skill_text,
            )
            .map_err(|e| {
                anyhow::anyhow!(
                    "Cannot parse SKILL.md frontmatter for outgoing revision '{}': {}",
                    alias.revision_id,
                    e
                )
            })?;
            parse_frontmatter_capabilities(&outgoing_frontmatter)?
        }
        None => {
            // Brand-new agent. The cursor decides whether first admission needs a
            // human: default (true) ⇒ empty baseline so all caps are "added" and
            // operator approval is required; false ⇒ no new-agent human gate (the
            // downstream completeness gate still applies, always fail-closed).
            if !gate_new_agents {
                return Ok(None);
            }
            Vec::new()
        }
    };

    let mut delta = autonoetic_types::capability::compute_capability_delta(
        &outgoing_capabilities,
        current_capabilities,
    );

    if !delta.has_broadening() {
        return Ok(None);
    }

    if matches!(mode, CapabilityDeltaGateMode::Evolving) {
        delta
            .broadened
            .retain(|b| !scope_change_within_existing_envelope(&b.previous_scope, &b.new_scope));
        if !delta.has_broadening() {
            return Ok(None);
        }
    }

    Ok(Some(delta))
}

fn scope_change_within_existing_envelope(previous_scope: &[String], new_scope: &[String]) -> bool {
    use std::collections::BTreeSet;

    let previous: BTreeSet<&str> = previous_scope.iter().map(String::as_str).collect();
    let current: BTreeSet<&str> = new_scope.iter().map(String::as_str).collect();

    let added: Vec<&str> = current
        .difference(&previous)
        .copied()
        .collect::<Vec<&str>>();

    if added.is_empty() {
        return true;
    }

    let wildcard_envelopes: Vec<&str> = previous
        .iter()
        .copied()
        .filter(|v| v.contains('*'))
        .collect();

    if wildcard_envelopes.is_empty() {
        return false;
    }

    added.iter().all(|candidate| {
        wildcard_envelopes
            .iter()
            .any(|pattern| wildcard_match(pattern, candidate))
    })
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == value;
    }

    let parts: Vec<&str> = pattern.split('*').collect();
    let mut cursor = 0usize;

    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 && !pattern.starts_with('*') {
            let Some(remaining) = value.get(cursor..) else {
                return false;
            };
            if !remaining.starts_with(part) {
                return false;
            }
            cursor += part.len();
            continue;
        }

        let Some(remaining) = value.get(cursor..) else {
            return false;
        };
        let Some(found) = remaining.find(part) else {
            return false;
        };
        cursor += found + part.len();
    }

    if !pattern.ends_with('*') {
        if let Some(last_non_empty) = parts.iter().rev().find(|p| !p.is_empty()) {
            return value.ends_with(last_non_empty);
        }
    }
    true
}

#[cfg(test)]
mod approval_execution_context_tests {
    use super::*;
    use crate::runtime::active_execution_registry::ActiveExecutionRegistry;

    #[test]
    fn run_context_supplies_workflow_task_and_root() {
        let rc = NativeToolRunContext {
            registry: ActiveExecutionRegistry::new(),
            root_session_id: "session-root".to_string(),
            workflow_id: Some("wf-abc".to_string()),
            task_id: Some("task-xyz".to_string()),
            session_id: "session-root/specialized_builder.default-child".to_string(),
            agent_id: "specialized_builder.default".to_string(),
            live_digest: None,
            live_report: None,
            user_id: None,
            artifact_id: None,
            sentinel_suppress_target: None,
            discovered_tools: None,
            wake_hints_map: None,
            wake_hint: None,
        };
        let (root, wf, task) =
            approval_execution_context(Some(&rc), Some(rc.session_id.as_str()));
        assert_eq!(root.as_deref(), Some("session-root"));
        assert_eq!(wf.as_deref(), Some("wf-abc"));
        assert_eq!(task.as_deref(), Some("task-xyz"));
    }

    #[test]
    fn session_only_falls_back_to_root_without_workflow_task() {
        let sid = "session-root/specialized_builder.default-child";
        let (root, wf, task) = approval_execution_context(None, Some(sid));
        assert_eq!(root.as_deref(), Some("session-root"));
        assert!(wf.is_none());
        assert!(task.is_none());
    }
}

#[cfg(test)]
mod capability_lenient_deser_tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct CapsOnly {
        #[serde(deserialize_with = "deserialize_capabilities_lenient")]
        capabilities: Vec<Capability>,
    }

    #[test]
    fn network_access_mistyped_scopes_normalized() {
        let j = r#"{"capabilities":[{"type":"NetworkAccess","scopes":["api.example.com"]}]}"#;
        let c: CapsOnly = serde_json::from_str(j).unwrap();
        assert!(matches!(
            c.capabilities.as_slice(),
            [Capability::NetworkAccess { hosts }] if hosts == &["api.example.com".to_string()]
        ));
    }

    #[test]
    fn read_access_hosts_normalized_to_scopes() {
        let j = r#"{"capabilities":[{"type":"ReadAccess","hosts":["/tmp"]}]}"#;
        let c: CapsOnly = serde_json::from_str(j).unwrap();
        assert!(matches!(
            c.capabilities.as_slice(),
            [Capability::ReadAccess { scopes }] if scopes == &["/tmp".to_string()]
        ));
    }

    #[test]
    fn string_shorthand_network_access_refused() {
        let j = r#"{"capabilities":["NetworkAccess"]}"#;
        let e = serde_json::from_str::<CapsOnly>(j).unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains("capabilities[0]"),
            "error should reference index, got: {msg}"
        );
        assert!(
            msg.contains("NetworkAccess"),
            "error should name capability, got: {msg}"
        );
        assert!(
            msg.contains("hosts"),
            "error should mention required field, got: {msg}"
        );
    }

    #[test]
    fn string_shorthand_code_execution_refused() {
        let j = r#"{"capabilities":["CodeExecution"]}"#;
        let e = serde_json::from_str::<CapsOnly>(j).unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains("CodeExecution"),
            "error should name capability, got: {msg}"
        );
        assert!(
            msg.contains("patterns"),
            "error should mention required field, got: {msg}"
        );
    }

    #[test]
    fn string_shorthand_credential_access_refused() {
        let j = r#"{"capabilities":["CredentialAccess"]}"#;
        let e = serde_json::from_str::<CapsOnly>(j).unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains("CredentialAccess"),
            "error should name capability, got: {msg}"
        );
        assert!(
            msg.contains("services"),
            "error should mention required field, got: {msg}"
        );
    }

    #[test]
    fn string_shorthand_read_access_refused() {
        let j = r#"{"capabilities":["ReadAccess"]}"#;
        let e = serde_json::from_str::<CapsOnly>(j).unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains("capabilities[0]"),
            "error should reference index, got: {msg}"
        );
        assert!(
            msg.contains("ReadAccess"),
            "error should name capability, got: {msg}"
        );
        assert!(
            msg.contains("scopes"),
            "error should mention required field, got: {msg}"
        );
    }

    #[test]
    fn string_shorthand_sandbox_functions_refused() {
        let j = r#"{"capabilities":["SandboxFunctions"]}"#;
        let e = serde_json::from_str::<CapsOnly>(j).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("SandboxFunctions"), "got: {msg}");
        assert!(msg.contains("allowed"), "got: {msg}");
    }

    #[test]
    fn string_shorthand_write_access_refused() {
        let j = r#"{"capabilities":["WriteAccess"]}"#;
        let e = serde_json::from_str::<CapsOnly>(j).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("WriteAccess"), "got: {msg}");
        assert!(msg.contains("scopes"), "got: {msg}");
    }

    #[test]
    fn string_shorthand_agent_message_refused() {
        let j = r#"{"capabilities":["AgentMessage"]}"#;
        let e = serde_json::from_str::<CapsOnly>(j).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("AgentMessage"), "got: {msg}");
        assert!(msg.contains("patterns"), "got: {msg}");
    }

    #[test]
    fn string_shorthand_emergency_stop_refused() {
        let j = r#"{"capabilities":["EmergencyStop"]}"#;
        let e = serde_json::from_str::<CapsOnly>(j).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("EmergencyStop"), "got: {msg}");
        assert!(msg.contains("tagged object"), "got: {msg}");
    }

    #[test]
    fn string_shorthand_skill_install_refused() {
        let j = r#"{"capabilities":["SkillInstall"]}"#;
        let e = serde_json::from_str::<CapsOnly>(j).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("SkillInstall"), "got: {msg}");
        assert!(msg.contains("allowed_sources"), "got: {msg}");
    }

    #[test]
    fn string_shorthand_scheduler_access_refused() {
        let j = r#"{"capabilities":["SchedulerAccess"]}"#;
        let e = serde_json::from_str::<CapsOnly>(j).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("SchedulerAccess"), "got: {msg}");
        assert!(msg.contains("patterns"), "got: {msg}");
    }

    #[test]
    fn string_shorthand_constitutional_proposal_refused() {
        let j = r#"{"capabilities":["ConstitutionalProposal"]}"#;
        let e = serde_json::from_str::<CapsOnly>(j).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("ConstitutionalProposal"), "got: {msg}");
        assert!(msg.contains("patterns"), "got: {msg}");
    }

    #[test]
    fn string_shorthand_user_profile_access_refused() {
        let j = r#"{"capabilities":["UserProfileAccess"]}"#;
        let e = serde_json::from_str::<CapsOnly>(j).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("UserProfileAccess"), "got: {msg}");
        assert!(msg.contains("scopes"), "got: {msg}");
    }

    #[test]
    fn scoped_network_access_object_accepted() {
        let j = r#"{"capabilities":[{"type":"NetworkAccess","hosts":["api.weather.com"]}]}"#;
        let c: CapsOnly = serde_json::from_str(j).unwrap();
        assert!(matches!(
            c.capabilities.as_slice(),
            [Capability::NetworkAccess { hosts }] if hosts == &["api.weather.com".to_string()]
        ));
    }

    #[test]
    fn scoped_network_access_wildcard_accepted() {
        let j = r#"{"capabilities":[{"type":"NetworkAccess","hosts":["*"]}]}"#;
        let c: CapsOnly = serde_json::from_str(j).unwrap();
        assert!(matches!(
            c.capabilities.as_slice(),
            [Capability::NetworkAccess { hosts }] if hosts == &["*".to_string()]
        ));
    }

    #[test]
    fn agent_spawn_string_rejected() {
        let j = r#"{"capabilities":["AgentSpawn"]}"#;
        let e = serde_json::from_str::<CapsOnly>(j).unwrap_err();
        assert!(e.to_string().contains("capabilities[0]"));
    }

    #[test]
    fn wildcard_match_supports_prefix_suffix() {
        assert!(wildcard_match("*.example.com", "api.example.com"));
        assert!(wildcard_match("scheduler.*", "scheduler.cron.create"));
        assert!(!wildcard_match("*.example.com", "example.org"));
    }

    #[test]
    fn wildcard_match_is_utf8_safe() {
        assert!(wildcard_match("pré*", "préfixe"));
        assert!(!wildcard_match("pré*", "postfixe"));
    }

    #[test]
    fn comparable_locked_layers_ignore_approval_scope() {
        let layers = vec![LockedLayerMount {
            layer_id: "layer_56:0f8a7".to_string(),
            digest: "sha256:testdigest".to_string(),
            mount_path: "/tmp/venv".to_string(),
            approval_scope: Some(autonoetic_types::layer::LayerApprovalScope {
                approved_hosts: vec![],
                built_by_agent_id: "packager.default".to_string(),
                captured_at: "2026-05-28T11:39:52.945382597+00:00".to_string(),
            }),
        }];

        let comparable = comparable_locked_layers(&layers);
        assert_eq!(
            comparable,
            vec![LockedLayerMount {
                layer_id: "layer_56:0f8a7".to_string(),
                digest: "sha256:testdigest".to_string(),
                mount_path: "/tmp/venv".to_string(),
                approval_scope: None,
            }]
        );
    }

    #[test]
    fn envelope_allows_new_entries_within_wildcard() {
        let previous = vec!["*.example.com".to_string()];
        let current = vec!["*.example.com".to_string(), "api.example.com".to_string()];
        assert!(scope_change_within_existing_envelope(&previous, &current));
    }

    #[test]
    fn envelope_rejects_new_entries_outside_wildcard() {
        let previous = vec!["*.example.com".to_string()];
        let current = vec!["*.example.com".to_string(), "api.evil.org".to_string()];
        assert!(!scope_change_within_existing_envelope(&previous, &current));
    }

    fn test_manifest() -> AgentManifest {
        AgentManifest {
            version: "1.0".to_string(),
            runtime: crate::runtime::install_contract::default_runtime_declaration(),
            agent: AgentIdentity {
                id: "specialized-builder.test".to_string(),
                name: "specialized-builder.test".to_string(),
                description: "test manifest".to_string(),
            },
            capabilities: vec![Capability::AgentRevision {
                patterns: vec!["*".to_string()],
            }],
            llm_preset: None,
            llm_overrides: None,
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            execution_mode: ExecutionMode::Reasoning,
            script_entry: None,
            script_input_mode: ScriptInputMode::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            agentskills_import: None,
            compression: None,
            open_web: false,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        }
    }

    #[test]
    fn create_from_intent_accepts_artifact_ref() {
        use autonoetic_types::artifact::{ArtifactRefRecord, ArtifactRefScopeType};
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let gateway_dir = dir.path().join(".gateway");
        let session_id = "sess-install";

        let content_store = crate::runtime::content_store::ContentStore::new(&gateway_dir).unwrap();
        let handle = content_store
            .write(b"#!/usr/bin/env python3\nprint('hello')\n")
            .unwrap();
        content_store
            .register_name(session_id, "main.py", &handle)
            .unwrap();

        let artifact_store = crate::artifact_store::ArtifactStore::new(&gateway_dir).unwrap();
        let inputs = vec!["main.py".to_string()];
        let entrypoints = vec!["main.py".to_string()];
        let bundle = artifact_store
            .build(&inputs, Some(&entrypoints), None, session_id)
            .unwrap();

        let gateway_store =
            Arc::new(crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap());
        let artifact_ref = "ar.testinstall01".to_string();
        gateway_store
            .create_artifact_ref(&ArtifactRefRecord {
                ref_id: artifact_ref.clone(),
                scope_type: ArtifactRefScopeType::Session,
                scope_id: session_id.to_string(),
                artifact_id: bundle.artifact_id.clone(),
                artifact_manifest_digest: bundle.artifact_manifest_digest.clone(),
                artifact_canonical_digest: bundle.artifact_canonical_digest.clone(),
                created_by_agent_id: "specialized-builder.test".to_string(),
                created_at: chrono::Utc::now().to_rfc3339(),
                expires_at: None,
                revoked_at: None,
            })
            .unwrap();

        let manifest = test_manifest();
        let policy = PolicyEngine::new(manifest.clone());
        let tool = AgentRevisionCreateFromIntentTool;
        let response = tool
            .execute(
                &manifest,
                &policy,
                dir.path(),
                Some(&gateway_dir),
                &serde_json::json!({
                    "agent_id": "weather-fetcher",
                    "artifact_ref": artifact_ref,
                    "description": "Fetch weather data",
                    "instructions": "# Weather Agent",
                    "capabilities": [
                        {"type": "ReadAccess", "scopes": ["*"]}
                    ]
                })
                .to_string(),
                Some(session_id),
                None,
                None,
                Some(gateway_store.clone()),
                None,
            )
            .unwrap();

        let response_json: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            response_json.get("ok").and_then(|value| value.as_bool()),
            Some(true)
        );
        assert_eq!(
            response_json
                .get("artifact_ref")
                .and_then(|value| value.as_str()),
            Some(artifact_ref.as_str())
        );

        let revision_id = response_json
            .get("revision_id")
            .and_then(|value| value.as_str())
            .unwrap();
        let revision = gateway_store
            .get_agent_revision(revision_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            revision.artifact_id.as_deref(),
            Some(bundle.artifact_id.as_str())
        );
        assert_eq!(revision.source_ref.as_deref(), Some("ar.testinstall01"));
    }

    #[test]
    fn create_from_intent_rejects_undeclared_network_host() {
        use autonoetic_types::artifact::{ArtifactRefRecord, ArtifactRefScopeType};
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let gateway_dir = dir.path().join(".gateway");
        let session_id = "sess-network-check";

        let code = b"#!/usr/bin/env python3\nimport urllib.request\nurllib.request.urlopen('https://api.open-meteo.com/v1/forecast')\n";
        let content_store = crate::runtime::content_store::ContentStore::new(&gateway_dir).unwrap();
        let handle = content_store.write(code).unwrap();
        content_store
            .register_name(session_id, "main.py", &handle)
            .unwrap();

        let artifact_store = crate::artifact_store::ArtifactStore::new(&gateway_dir).unwrap();
        let inputs = vec!["main.py".to_string()];
        let entrypoints = vec!["main.py".to_string()];
        let bundle = artifact_store
            .build(&inputs, Some(&entrypoints), None, session_id)
            .unwrap();

        let gateway_store =
            Arc::new(crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap());
        let artifact_ref = "ar.networkcheck01".to_string();
        gateway_store
            .create_artifact_ref(&ArtifactRefRecord {
                ref_id: artifact_ref.clone(),
                scope_type: ArtifactRefScopeType::Session,
                scope_id: session_id.to_string(),
                artifact_id: bundle.artifact_id.clone(),
                artifact_manifest_digest: bundle.artifact_manifest_digest.clone(),
                artifact_canonical_digest: bundle.artifact_canonical_digest.clone(),
                created_by_agent_id: "specialized-builder.test".to_string(),
                created_at: chrono::Utc::now().to_rfc3339(),
                expires_at: None,
                revoked_at: None,
            })
            .unwrap();

        let manifest = test_manifest();
        let policy = PolicyEngine::new(manifest.clone());
        let tool = AgentRevisionCreateFromIntentTool;
        let response = tool
            .execute(
                &manifest,
                &policy,
                dir.path(),
                Some(&gateway_dir),
                &serde_json::json!({
                    "agent_id": "weather-fetcher",
                    "artifact_ref": artifact_ref,
                    "description": "Fetch weather data",
                    "instructions": "# Weather Agent",
                    "execution_mode": "script",
                    "script_entry": "main.py",
                    "capabilities": [
                        {"type": "NetworkAccess", "hosts": ["wrong.host.com"]}
                    ]
                })
                .to_string(),
                Some(session_id),
                None,
                None,
                Some(gateway_store.clone()),
                None,
            )
            .unwrap();

        let response_json: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            response_json.get("ok").and_then(|value| value.as_bool()),
            Some(false)
        );
        let message = response_json
            .get("message")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        assert!(
            message.contains("api.open-meteo.com"),
            "message should mention undeclared host: {message}"
        );
        assert!(
            message.contains("Undeclared hosts"),
            "message should mention undeclared hosts: {message}"
        );
    }

    #[test]
    fn create_from_intent_accepts_declared_network_hosts() {
        use autonoetic_types::artifact::{ArtifactRefRecord, ArtifactRefScopeType};
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let gateway_dir = dir.path().join(".gateway");
        let session_id = "sess-network-ok";

        let code = b"#!/usr/bin/env python3\nimport urllib.request\nurllib.request.urlopen('https://api.open-meteo.com/v1/forecast')\n";
        let content_store = crate::runtime::content_store::ContentStore::new(&gateway_dir).unwrap();
        let handle = content_store.write(code).unwrap();
        content_store
            .register_name(session_id, "main.py", &handle)
            .unwrap();

        let artifact_store = crate::artifact_store::ArtifactStore::new(&gateway_dir).unwrap();
        let inputs = vec!["main.py".to_string()];
        let entrypoints = vec!["main.py".to_string()];
        let bundle = artifact_store
            .build(&inputs, Some(&entrypoints), None, session_id)
            .unwrap();

        let gateway_store =
            Arc::new(crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap());
        let artifact_ref = "ar.networkok01".to_string();
        gateway_store
            .create_artifact_ref(&ArtifactRefRecord {
                ref_id: artifact_ref.clone(),
                scope_type: ArtifactRefScopeType::Session,
                scope_id: session_id.to_string(),
                artifact_id: bundle.artifact_id.clone(),
                artifact_manifest_digest: bundle.artifact_manifest_digest.clone(),
                artifact_canonical_digest: bundle.artifact_canonical_digest.clone(),
                created_by_agent_id: "specialized-builder.test".to_string(),
                created_at: chrono::Utc::now().to_rfc3339(),
                expires_at: None,
                revoked_at: None,
            })
            .unwrap();

        let manifest = test_manifest();
        let policy = PolicyEngine::new(manifest.clone());
        let tool = AgentRevisionCreateFromIntentTool;
        let response = tool
            .execute(
                &manifest,
                &policy,
                dir.path(),
                Some(&gateway_dir),
                &serde_json::json!({
                    "agent_id": "weather-fetcher",
                    "artifact_ref": artifact_ref,
                    "description": "Fetch weather data",
                    "instructions": "# Weather Agent",
                    "execution_mode": "script",
                    "script_entry": "main.py",
                    "capabilities": [
                        {"type": "NetworkAccess", "hosts": ["api.open-meteo.com"]}
                    ]
                })
                .to_string(),
                Some(session_id),
                None,
                None,
                Some(gateway_store.clone()),
                None,
            )
            .unwrap();

        let response_json: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            response_json.get("ok").and_then(|value| value.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn duplicate_create_from_intent_request_coalesces_when_install_reservation_exists() {
        use autonoetic_types::artifact::{ArtifactRefRecord, ArtifactRefScopeType};
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        let gateway_dir = agents_dir.join(".gateway");
        let session_id = "sess-install";

        let content_store = crate::runtime::content_store::ContentStore::new(&gateway_dir).unwrap();
        let handle = content_store
            .write(b"#!/usr/bin/env python3\nprint('hello')\n")
            .unwrap();
        content_store
            .register_name(session_id, "main.py", &handle)
            .unwrap();

        let artifact_store = crate::artifact_store::ArtifactStore::new(&gateway_dir).unwrap();
        let inputs = vec!["main.py".to_string()];
        let entrypoints = vec!["main.py".to_string()];
        let bundle = artifact_store
            .build(&inputs, Some(&entrypoints), None, session_id)
            .unwrap();

        let gateway_store =
            Arc::new(crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap());
        let artifact_ref = "ar.testinstall01".to_string();
        gateway_store
            .create_artifact_ref(&ArtifactRefRecord {
                ref_id: artifact_ref.clone(),
                scope_type: ArtifactRefScopeType::Session,
                scope_id: session_id.to_string(),
                artifact_id: bundle.artifact_id.clone(),
                artifact_manifest_digest: bundle.artifact_manifest_digest.clone(),
                artifact_canonical_digest: bundle.artifact_canonical_digest.clone(),
                created_by_agent_id: "specialized-builder.test".to_string(),
                created_at: chrono::Utc::now().to_rfc3339(),
                expires_at: None,
                revoked_at: None,
            })
            .unwrap();

        let config = GatewayConfig {
            agents_dir: agents_dir.clone(),
            ..GatewayConfig::default()
        };
        let workflow = crate::scheduler::ensure_workflow_for_root_session(
            &config,
            Some(gateway_store.as_ref()),
            session_id,
            Some("specialized-builder.test"),
        )
        .unwrap();
        let spec = crate::scheduler::single_flight::build_spec(
            &workflow.workflow_id,
            "install",
            "weather-fetcher",
            Some(artifact_ref.clone()),
            None,
        );
        match crate::scheduler::single_flight::try_acquire_reservation(
            &config,
            Some(gateway_store.as_ref()),
            &spec,
            None,
            None,
        )
        .unwrap()
        {
            crate::scheduler::single_flight::AcquireOutcome::Acquired(_) => {}
            crate::scheduler::single_flight::AcquireOutcome::Coalesced(_) => {
                panic!("seed reservation must acquire")
            }
        }

        let manifest = test_manifest();
        let policy = PolicyEngine::new(manifest.clone());
        let tool = AgentRevisionCreateFromIntentTool;
        let response = tool
            .execute(
                &manifest,
                &policy,
                dir.path(),
                Some(&gateway_dir),
                &serde_json::json!({
                    "agent_id": "weather-fetcher",
                    "artifact_ref": artifact_ref,
                    "description": "Fetch weather data",
                    "instructions": "# Weather Agent",
                    "capabilities": [
                        {"type": "ReadAccess", "scopes": ["*"]}
                    ]
                })
                .to_string(),
                Some(session_id),
                None,
                Some(&config),
                Some(gateway_store),
                None,
            )
            .unwrap();

        let response_json: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response_json["ok"].as_bool(), Some(true));
        assert_eq!(response_json["status"].as_str(), Some("coalesced"));
        assert_eq!(response_json["dedupe_key"].as_str(), Some(spec.dedupe_key.as_str()));
        assert_eq!(response_json["retry_advice"].as_str(), Some("wait"));
    }
}

#[cfg(test)]
mod capability_schema_tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Drift guard: the `capability_oneof_schema()` oneOf branches must cover
    /// every `Capability` variant exactly (by serialized `type` tag), and must
    /// not advertise variants the enum no longer has. When you add or rename a
    /// `Capability` variant, update the enum, this list, and
    /// `capability_oneof_schema`.
    #[test]
    fn capability_oneof_schema_covers_all_variants() {
        let representatives: Vec<Capability> = vec![
            Capability::SandboxFunctions { allowed: vec![] },
            Capability::ReadAccess { scopes: vec![] },
            Capability::WriteAccess { scopes: vec![] },
            Capability::NetworkAccess { hosts: vec![] },
            Capability::AgentSpawn { max_children: 0, max_spawn_depth: 0 },
            Capability::AgentMessage { patterns: vec![] },
            Capability::BackgroundReevaluation { min_interval_secs: 1, allow_reasoning: false },
            Capability::CodeExecution { patterns: vec![], commands: vec![] },
            Capability::EmergencyStop,
            Capability::AgentRevision { patterns: vec![] },
            Capability::Evaluation { patterns: vec![] },
            Capability::ApprovalQueue { patterns: vec![] },
            Capability::SchedulerSignal { patterns: vec![] },
            Capability::CredentialAccess { services: vec![] },
            Capability::UserProfileAccess { scopes: vec![] },
            Capability::SchedulerAccess { patterns: vec![] },
            Capability::SkillInstall { allowed_sources: vec![] },
            Capability::ConstitutionalProposal { patterns: vec![] },
            Capability::ReasoningAudit { targets: vec![] },
            Capability::BudgetNoPriceAvailableAllow,
            Capability::GithubIssueCreate { patterns: vec![] },
            Capability::SecurityRedTeam,
            Capability::CapsuleExport,
            Capability::WikiContribute,
            Capability::PlanFrameAccess { patterns: vec![] },
            Capability::PromoteWith { agent_id: String::new(), capabilities: vec![] },
        ];

        let expected: BTreeSet<String> = representatives
            .iter()
            .map(|c| {
                let v = serde_json::to_value(c).unwrap();
                v["type"].as_str().unwrap().to_string()
            })
            .collect();

        let schema = capability_oneof_schema();
        let branches = schema.as_array().expect("oneOf schema is an array");
        let actual: BTreeSet<String> = branches
            .iter()
            .map(|b| {
                b["properties"]["type"]["const"]
                    .as_str()
                    .unwrap_or_else(|| panic!("branch missing type const: {b}"))
                    .to_string()
            })
            .collect();

        let missing: Vec<_> = expected.difference(&actual).cloned().collect();
        let extra: Vec<_> = actual.difference(&expected).cloned().collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "schema/enum drift — missing from schema: {missing:?}, extra in schema: {extra:?}"
        );
    }

    /// Every oneOf branch must require `type` (the serde discriminator) and the
    /// fields that have no `#[serde(default)]` in the enum. This catches the
    /// original bug: a branch that omits a required field, letting the LLM
    /// produce JSON serde then rejects.
    #[test]
    fn capability_oneof_schema_enforces_required_fields() {
        let schema = capability_oneof_schema();
        let branches = schema.as_array().expect("oneOf schema is an array");
        assert!(branches.len() > 10);

        // type tag → set of fields that MUST be required (no serde default in the enum).
        let must_require: &[(&str, &[&str])] = &[
            ("SandboxFunctions", &["type", "allowed"]),
            ("ReadAccess", &["type", "scopes"]),
            ("WriteAccess", &["type", "scopes"]),
            ("NetworkAccess", &["type", "hosts"]),
            ("AgentSpawn", &["type", "max_children"]),
            (
                "BackgroundReevaluation",
                &["type", "min_interval_secs", "allow_reasoning"],
            ),
            ("UserProfileAccess", &["type", "scopes"]),
            ("PromoteWith", &["type", "capabilities"]),
        ];

        for (tag, required_fields) in must_require {
            let branch = branches
                .iter()
                .find(|b| b["properties"]["type"]["const"].as_str() == Some(*tag))
                .unwrap_or_else(|| panic!("no schema branch for {tag}"));
            let required: Vec<String> = branch["required"]
                .as_array()
                .unwrap_or_else(|| panic!("{tag} branch missing `required`"))
                .iter()
                .map(|r| r.as_str().unwrap().to_string())
                .collect();
            for field in *required_fields {
                assert!(
                    required.iter().any(|r| r == *field),
                    "{tag} schema branch must require `{field}` (got required: {required:?})"
                );
            }
        }
    }
}

#[cfg(test)]
mod network_access_host_validation_tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::runtime::network_host_contract::{
        capability_host_allows, validate_network_host_contract,
    };

    #[test]
    fn capability_host_allows_exact_match() {
        assert!(capability_host_allows("api.example.com", "api.example.com"));
        assert!(!capability_host_allows("api.example.com", "other.example.com"));
    }

    #[test]
    fn capability_host_allows_bare_domain_covers_subdomains() {
        assert!(capability_host_allows("example.com", "example.com"));
        assert!(capability_host_allows("example.com", "api.example.com"));
        assert!(capability_host_allows("example.com", "deep.api.example.com"));
        assert!(!capability_host_allows("example.com", "otherexample.com"));
    }

    #[test]
    fn capability_host_allows_wildcard_subdomain() {
        assert!(capability_host_allows("*.example.com", "api.example.com"));
        assert!(!capability_host_allows("*.example.com", "example.com"));
    }

    #[test]
    fn capability_host_allows_universal_wildcard() {
        assert!(capability_host_allows("*", "anything.example.com"));
    }

    #[test]
    fn validate_network_access_hosts_accepts_exact_match() {
        let mut file_map = BTreeMap::new();
        file_map.insert(
            "main.py".to_string(),
            b"import urllib.request\nurllib.request.urlopen('https://api.open-meteo.com/v1')".to_vec(),
        );
        let caps = vec![Capability::NetworkAccess {
            hosts: vec!["api.open-meteo.com".to_string()],
        }];
        assert!(validate_network_host_contract(&caps, &file_map, false).is_ok());
    }

    #[test]
    fn validate_network_access_hosts_rejects_undeclared_host() {
        let mut file_map = BTreeMap::new();
        file_map.insert(
            "main.py".to_string(),
            b"import urllib.request\nurllib.request.urlopen('https://api.open-meteo.com/v1')".to_vec(),
        );
        let caps = vec![Capability::NetworkAccess {
            hosts: vec!["other.example.com".to_string()],
        }];
        let err = validate_network_host_contract(&caps, &file_map, false).unwrap_err();
        assert!(err.undeclared_hosts.iter().any(|h| h.contains("open-meteo")));
    }

    #[test]
    fn validate_network_access_hosts_rejects_wildcard_without_open_web() {
        let mut file_map = BTreeMap::new();
        file_map.insert(
            "main.py".to_string(),
            b"import urllib.request\nurllib.request.urlopen('https://api.open-meteo.com/v1')".to_vec(),
        );
        let caps = vec![Capability::NetworkAccess {
            hosts: vec!["*".to_string()],
        }];
        assert!(validate_network_host_contract(&caps, &file_map, false).is_err());
    }

    #[test]
    fn validate_network_access_hosts_allows_wildcard_with_open_web() {
        let mut file_map = BTreeMap::new();
        file_map.insert(
            "main.py".to_string(),
            b"import urllib.request\nurllib.request.urlopen('https://api.open-meteo.com/v1')".to_vec(),
        );
        let caps = vec![Capability::NetworkAccess {
            hosts: vec!["*".to_string()],
        }];
        assert!(validate_network_host_contract(&caps, &file_map, true).is_ok());
    }

    #[test]
    fn validate_network_access_hosts_ignores_non_source_files() {
        let mut file_map = BTreeMap::new();
        file_map.insert(
            "data.bin".to_string(),
            b"https://api.open-meteo.com/v1".to_vec(),
        );
        let caps = vec![Capability::NetworkAccess {
            hosts: vec!["other.example.com".to_string()],
        }];
        assert!(validate_network_host_contract(&caps, &file_map, false).is_ok());
    }
}
