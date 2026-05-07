//! Bootstrap agents from the agents directory into the gateway store.
//!
//! Scans `config.agents_dir` for agent bundles (directories with `SKILL.md`),
//! creates revisions from their content, and auto-promotes them. Skips agents
//! whose content hash matches an existing revision (content-addressed dedup). Merges preset-level LLM config into the
//! agent's `SKILL.md`: always overrides `provider`, `model`, `temperature`;
//! fills missing `base_url`, `thinking`, `chat_only`, `api_key_env`.
//! Also materializes constitution snapshots in `.gateway/constitution/`.

use crate::scheduler::gateway_store::GatewayStore;
use anyhow::Result;
use autonoetic_types::agent_revision::{AgentRevisionRecord, AgentRevisionStatus};
use autonoetic_types::config::{GatewayConfig, LlmPreset};
use autonoetic_types::id_format::mint_hashed_prefixed_id;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// When `AUTONOETIC_VAULT_KEY` / `AUTONOETIC_VAULT_KEY_PATH` are unset, ensures
/// `{agents_dir}/.gateway/vault.key` exists (see [`crate::vault::ensure_default_key`]).
///
/// Returns `true` if this invocation created a new key file.
pub fn ensure_vault_key_for_bootstrap_workspace(config: &GatewayConfig) -> Result<bool> {
    let key_path = config.agents_dir.join(".gateway").join("vault.key");
    let had_file_before = key_path.exists();
    if std::env::var("AUTONOETIC_VAULT_KEY").is_ok()
        || std::env::var("AUTONOETIC_VAULT_KEY_PATH").is_ok()
    {
        crate::vault::ensure_default_key(&config.agents_dir)?;
        return Ok(false);
    }
    crate::vault::ensure_default_key(&config.agents_dir)?;
    let created = !had_file_before && key_path.exists();
    if created {
        tracing::info!(
            target: "bootstrap",
            path = %key_path.display(),
            "Created default vault master key (no AUTONOETIC_VAULT_KEY / AUTONOETIC_VAULT_KEY_PATH)"
        );
    }
    Ok(created)
}

/// Bootstrap all agents from `config.agents_dir` into the gateway store.
/// Returns the number of agents activated.
pub fn bootstrap_agents(config: &GatewayConfig, gateway_dir: &Path) -> Result<usize> {
    ensure_vault_key_for_bootstrap_workspace(config)?;
    write_gateway_identity(gateway_dir)?;
    bootstrap_constitution_snapshot(config, gateway_dir)?;

    let store = GatewayStore::open(gateway_dir)?;
    let mut activated = 0usize;

    for entry in std::fs::read_dir(&config.agents_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let agent_dir = entry.path();
        let skill_path = agent_dir.join("SKILL.md");
        if !skill_path.exists() {
            continue;
        }
        let agent_id = agent_dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("Invalid agent dir name: {}", agent_dir.display()))?
            .to_string();

        if bootstrap_agent_inner(config, gateway_dir, &store, &agent_id)? {
            activated += 1;
        }
    }

    Ok(activated)
}

/// Bootstrap a single named agent from `config.agents_dir` into the gateway store.
/// Returns `true` if a new revision was activated, `false` if skipped (already exists).
pub fn bootstrap_single_agent(
    config: &GatewayConfig,
    gateway_dir: &Path,
    agent_id: &str,
) -> Result<bool> {
    ensure_vault_key_for_bootstrap_workspace(config)?;
    write_gateway_identity(gateway_dir)?;
    bootstrap_constitution_snapshot(config, gateway_dir)?;
    let store = GatewayStore::open(gateway_dir)?;
    bootstrap_agent_inner(config, gateway_dir, &store, agent_id)
}

/// Inner bootstrap logic shared by `bootstrap_agents` and `bootstrap_single_agent`.
fn bootstrap_agent_inner(
    config: &GatewayConfig,
    gateway_dir: &Path,
    store: &GatewayStore,
    agent_id: &str,
) -> Result<bool> {
    let agent_dir = config.agents_dir.join(agent_id);
    let skill_path = agent_dir.join("SKILL.md");

    anyhow::ensure!(
        skill_path.exists(),
        "SKILL.md not found for agent '{}' at {}",
        agent_id,
        skill_path.display()
    );

    let skill_content = std::fs::read(&skill_path)?;
    let skill_text = String::from_utf8_lossy(&skill_content);
    let (parsed_manifest, _instructions) = crate::runtime::parser::SkillParser::parse(&skill_text)
        .map_err(|e| anyhow::anyhow!("Failed to parse SKILL.md for '{}': {}", agent_id, e))?;

    let lock_rel_path = &parsed_manifest.runtime.runtime_lock;
    let lock_path = agent_dir.join(lock_rel_path);
    let mut lock_content = std::fs::read(&lock_path).map_err(|e| {
        anyhow::anyhow!(
            "Missing runtime.lock '{}' for agent '{}': {}",
            lock_rel_path,
            agent_id,
            e
        )
    })?;

    let lock_text = String::from_utf8_lossy(&lock_content);
    if lock_text.contains(crate::runtime::install_contract::PLACEHOLDER_SHA) {
        let replaced = lock_text.replace(
            crate::runtime::install_contract::PLACEHOLDER_SHA,
            crate::runtime::install_contract::GATEWAY_BUILD_SHA256,
        );
        lock_content = replaced.into_bytes();
    }

    let manifest_hash = format!("sha256:{:x}", Sha256::digest(&skill_content));
    let runtime_lock_hash = format!("sha256:{:x}", Sha256::digest(&lock_content));

    let mut file_map: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    collect_files(&agent_dir, &agent_dir, &mut file_map)?;
    file_map.insert(lock_rel_path.clone(), lock_content.clone());

    // Merge preset-level `thinking` into the agent's llm_config if the agent
    // doesn't already specify thinking. This ensures bootstrapped revisions
    // carry the preset's thinking config as part of their stored manifest.
    let presets = &config.llm_presets;
    let base_name = agent_id.rsplit_once('.').map(|(base, _)| base.to_string());
    let preset_name = config
        .llm_preset_mapping
        .get(agent_id)
        .or_else(|| {
            base_name
                .as_ref()
                .and_then(|b| config.llm_preset_mapping.get(b))
        })
        .or_else(|| config.llm_preset_mapping.get("default"));

    if let Some(name) = preset_name {
        if let Some(preset) = presets.get(name.as_str()) {
            if let Some(modified) = merge_preset_into_skill(&skill_text, preset) {
                let modified_bytes = modified.into_bytes();
                file_map.insert("SKILL.md".to_string(), modified_bytes.clone());
                std::fs::write(&skill_path, &modified_bytes)?;
            }
        }
    }

    let mut hasher = Sha256::new();
    for (path, bytes) in &file_map {
        hasher.update(path.as_bytes());
        hasher.update([0_u8]);
        hasher.update(bytes);
        hasher.update([0_u8]);
    }
    let revision_digest_hex = format!("{:x}", hasher.finalize());
    let revision_id = format!("rev_sha256:{}", revision_digest_hex);
    let content_digest = format!("sha256:{}", revision_digest_hex);

    // Skip if this exact revision already exists
    if store.get_agent_revision(&revision_id)?.is_some() {
        return Ok(false);
    }

    let revision_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join(agent_id)
        .join(&revision_id);

    if !revision_dir.exists() {
        for (rel_path, bytes) in &file_map {
            let dest = revision_dir.join(rel_path);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, bytes)?;
        }

        if let Some(ref entry) = parsed_manifest.script_entry {
            let entry_path = revision_dir.join(entry);
            if entry_path.is_file() {
                let mut perms = std::fs::metadata(&entry_path)?.permissions();
                perms.set_mode(perms.mode() | 0o111);
                std::fs::set_permissions(&entry_path, perms)?;
            }
        }
    }

    let now = chrono::Utc::now().to_rfc3339();
    let rev = AgentRevisionRecord {
        revision_id: revision_id.clone(),
        agent_id: agent_id.to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest,
        runtime_lock_hash,
        manifest_hash,
        created_at: now.clone(),
        created_by_type: "bootstrap".to_string(),
        created_by_id: "cli".to_string(),
        source_kind: "bootstrap".to_string(),
        source_ref: None,
        origin_node_id: config.node_id.clone(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Candidate,
        metadata_json: serde_json::json!({
            "summary": "Bootstrapped from reference agent bundle",
        }),
        short_id: String::new(),
        signature: None,
        signer_id: None,
    };

    store.insert_agent_revision_transactional(&rev)?;

    let promotion_id =
        mint_hashed_prefixed_id("prom-", &format!("{}-{}-{}", agent_id, revision_id, now));

    store.atomic_promote(
        agent_id,
        &revision_id,
        &promotion_id,
        "bootstrap",
        "cli",
        Some("Auto-promoted during agent bootstrap"),
        None,
    )?;

    Ok(true)
}

fn collect_files(base: &Path, current: &Path, out: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(base, &path, out)?;
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

/// Materialize the active constitution into `.gateway/constitution/` so the
/// runtime directory carries an immutable local snapshot plus active pointers.
pub fn bootstrap_constitution_snapshot(config: &GatewayConfig, gateway_dir: &Path) -> Result<()> {
    #[derive(serde::Serialize)]
    struct ActiveConstitutionSnapshot {
        constitution_version: String,
        constitution_digest: String,
        source_path: String,
        lock_path: String,
        lock_signer_id: String,
        origin_source_path: String,
        origin_lock_path: String,
        bootstrapped_at: String,
    }

    crate::constitution_digest::initialize_constitution(config)?;
    crate::constitution_digest::verify_constitution_lock_integrity()?;

    let version = crate::constitution_digest::constitution_version().to_string();
    let digest = crate::constitution_digest::constitution_digest().to_string();
    let source_rel = format!(".gateway/constitution/versions/{version}/constitution.md");
    let lock_rel =
        format!(".gateway/constitution/versions/{version}/gateway-constitution.lock.json");

    let constitution_root = gateway_dir.join("constitution");
    let version_dir = constitution_root.join("versions").join(&version);
    std::fs::create_dir_all(&version_dir)?;

    std::fs::write(
        version_dir.join("constitution.md"),
        crate::constitution_digest::constitution_text().as_ref(),
    )?;

    let mut lock_snapshot = crate::constitution_digest::constitution_lock()
        .as_ref()
        .clone();
    lock_snapshot.constitution_source = source_rel.clone();
    let gateway_key = crate::runtime::crypto::GatewayIdentityKey::load_or_generate(gateway_dir)?;
    let lock_signer_id = format!("gateway:{}", gateway_key.fingerprint());
    let signature_payload =
        crate::constitution_digest::constitution_lock_signature_payload(&lock_snapshot)?;
    lock_snapshot.signature = Some(crate::constitution_digest::ConstitutionLockSignature {
        algorithm: "ed25519".to_string(),
        signer_id: lock_signer_id.clone(),
        signature_b64: gateway_key.sign(&signature_payload),
    });
    let lock_json = serde_json::to_string_pretty(&lock_snapshot)?;
    std::fs::write(
        version_dir.join("gateway-constitution.lock.json"),
        format!("{lock_json}\n"),
    )?;

    std::fs::write(constitution_root.join("CURRENT"), format!("{version}\n"))?;

    let active = ActiveConstitutionSnapshot {
        constitution_version: version,
        constitution_digest: digest,
        source_path: source_rel,
        lock_path: lock_rel,
        lock_signer_id,
        origin_source_path: normalize_path_label(&config.constitution.source_path),
        origin_lock_path: normalize_path_label(&config.constitution.lock_path),
        bootstrapped_at: chrono::Utc::now().to_rfc3339(),
    };
    let active_json = serde_json::to_string_pretty(&active)?;
    std::fs::write(
        constitution_root.join("ACTIVE.json"),
        format!("{active_json}\n"),
    )?;

    Ok(())
}

fn normalize_path_label(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Merges preset-level `llm_config` fields into the SKILL.md file,
/// preserving the original YAML structure. Always overrides `provider`,
/// `model`, and `temperature` from the preset (when set). Fills missing
/// `base_url`, `thinking`, `chat_only`, and `api_key_env` only if not
/// already present in the agent's config.
/// Returns `None` if no modification was needed or if the frontmatter couldn't
/// be parsed.
fn merge_preset_into_skill(skill_text: &str, preset: &LlmPreset) -> Option<String> {
    let (fm_start, fm_end) = {
        let t = skill_text.trim_start();
        if !t.starts_with("---") {
            return None;
        }
        let rest = &t[3..];
        let first_nl = rest.find(|c: char| c == '\n' || c == '\r')?;
        let after_first = &rest[first_nl..];
        let end = after_first.find("\n---")?;
        (3 + first_nl, 3 + first_nl + end)
    };

    let frontmatter = &skill_text[fm_start..fm_end];

    let mut yaml: serde_yaml::Value = serde_yaml::from_str(frontmatter).ok()?;

    let llm_config = {
        if yaml.get("llm_config").is_some() {
            yaml.get_mut("llm_config")
        } else {
            yaml.get_mut("metadata")
                .and_then(|m| m.get_mut("autonoetic"))
                .and_then(|a| a.get_mut("llm_config"))
        }
    };

    let mut modified = false;

    if let Some(cfg) = llm_config {
        if let Some(map) = cfg.as_mapping_mut() {
            let yaml_str = |s: &str| serde_yaml::Value::String(s.to_string());

            // Always override these (infrastructure-level concerns controlled by config.yaml)
            if let Some(ref provider) = preset.provider {
                map.insert(yaml_str("provider"), serde_yaml::to_value(provider).ok()?);
                modified = true;
            }
            if let Some(ref model) = preset.model {
                map.insert(yaml_str("model"), serde_yaml::to_value(model).ok()?);
                modified = true;
            }
            if let Some(temperature) = preset.temperature {
                map.insert(
                    yaml_str("temperature"),
                    serde_yaml::to_value(temperature).ok()?,
                );
                modified = true;
            }

            // Fill-missing for these (agent-specific or optional)
            let base_url_key = yaml_str("base_url");
            let has_base_url = map.get(&base_url_key).map_or(false, |v| !v.is_null());
            if !has_base_url {
                if let Some(ref base_url) = preset.base_url {
                    map.insert(base_url_key, serde_yaml::to_value(base_url).ok()?);
                    modified = true;
                }
            }

            let chat_only_key = yaml_str("chat_only");
            let has_chat_only = map.get(&chat_only_key).map_or(false, |v| !v.is_null());
            if !has_chat_only {
                if let Some(chat_only) = preset.chat_only {
                    map.insert(chat_only_key, serde_yaml::to_value(chat_only).ok()?);
                    modified = true;
                }
            }

            let api_key_env_key = yaml_str("api_key_env");
            let has_api_key_env = map.get(&api_key_env_key).map_or(false, |v| !v.is_null());
            if !has_api_key_env {
                if let Some(ref api_key_env) = preset.api_key_env {
                    map.insert(api_key_env_key, serde_yaml::to_value(api_key_env).ok()?);
                    modified = true;
                }
            }

            let thinking_key = yaml_str("thinking");
            let has_thinking = map.get(&thinking_key).map_or(false, |v| !v.is_null());
            if !has_thinking {
                if let Some(ref thinking) = preset.thinking {
                    map.insert(thinking_key, serde_yaml::to_value(thinking).ok()?);
                    modified = true;
                }
            }
        }
    }

    if !modified {
        return None;
    }

    let new_frontmatter = serde_yaml::to_string(&yaml).ok()?;
    let body = &skill_text[fm_end + 4..];
    Some(format!("---\n{}---{}\n", new_frontmatter, body))
}

fn write_gateway_identity(gateway_dir: &Path) -> Result<()> {
    #[derive(serde::Serialize)]
    struct GatewayIdentity {
        version: String,
        sha256: String,
        binary_sha256: Option<String>,
        build_tag: Option<String>,
    }

    let binary_sha = crate::runtime::install_contract::running_binary_sha256().ok();

    std::fs::create_dir_all(gateway_dir)?;

    let identity = GatewayIdentity {
        version: crate::runtime::install_contract::gateway_version(),
        sha256: crate::runtime::install_contract::GATEWAY_BUILD_SHA256.to_string(),
        binary_sha256: binary_sha,
        build_tag: Some(crate::runtime::install_contract::GATEWAY_BUILD_TAG.to_string()),
    };

    let json = serde_json::to_string_pretty(&identity)?;
    std::fs::write(gateway_dir.join("gateway.json"), json)?;
    Ok(())
}
