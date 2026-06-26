//! Bootstrap agents from the agents directory into the gateway store.
//!
//! Scans `config.agents_dir` for agent bundles (directories with `SKILL.md`),
//! creates revisions from their content, and auto-promotes them. Skips agents
//! whose content hash matches an existing revision (content-addressed dedup). Merges preset-level LLM config into the
//! agent's `SKILL.md`: always overrides `provider`, `model`, `temperature`, and
//! `thinking` (when the preset defines it); fills missing `base_url`, `chat_only`,
//! `api_key_env`.
//! Also materializes constitution snapshots in `.gateway/constitution/`.

use crate::scheduler::gateway_store::GatewayStore;
use anyhow::Result;
use autonoetic_types::agent::ExecutionMode;
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
    bootstrap_sdk_snapshot(gateway_dir)?;
    bootstrap_wiki_snapshot(gateway_dir)?;
    crate::sandbox::init_sdk_deployed_path(gateway_dir);

    let store = GatewayStore::open(gateway_dir)?;

    // Startup reaper: clear orphan checkpoint files left behind by a crash or
    // restart during approval reject/cancel (#607). Best-effort — a failure
    // never blocks startup.
    if let Err(e) = crate::runtime::checkpoint::reap_orphan_checkpoints(config, &store) {
        tracing::warn!(target: "checkpoint", error = %e, "Startup checkpoint reaper failed");
    }

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
    bootstrap_sdk_snapshot(gateway_dir)?;
    bootstrap_wiki_snapshot(gateway_dir)?;
    crate::sandbox::init_sdk_deployed_path(gateway_dir);
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
    let (mut parsed_manifest, _instructions) =
        crate::runtime::parser::SkillParser::parse(&skill_text)
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
    let current_build_sha = crate::runtime::install_contract::GATEWAY_BUILD_SHA256;
    let needs_rewrite = if lock_text.contains(crate::runtime::install_contract::PLACEHOLDER_SHA) {
        // The bundled template left a `replace-me` marker — materialize it.
        true
    } else {
        // The on-disk lock has a real SHA but it isn't the current gateway's
        // build SHA. The agent was installed/scaffolded against an older
        // binary and `--overwrite` is supposed to bring it in sync. Refresh
        // it so the next drift check has something to match against.
        let lock_sha = lock_text
            .lines()
            .find_map(|line| {
                let trimmed = line.trim_start();
                trimmed
                    .strip_prefix("sha256:")
                    .and_then(|v| v.trim().strip_prefix('"').map(|s| s.to_string()))
            })
            .unwrap_or_default();
        lock_sha != current_build_sha
    };
    if needs_rewrite {
        let replaced = lock_text.replace(
            crate::runtime::install_contract::PLACEHOLDER_SHA,
            current_build_sha,
        );
        // Also replace any other stale `sha256: "<digest>"` line under the
        // `gateway:` block with the current build SHA. The placeholder
        // branch above is the common path; this handles agents that were
        // installed under a previous binary and never re-overwritten.
        let mut rewritten = String::with_capacity(replaced.len());
        let mut in_gateway_block = false;
        for line in replaced.lines() {
            if line.trim_start().starts_with("gateway:") {
                in_gateway_block = true;
            } else if line.chars().next().map_or(false, |c| !c.is_whitespace()) {
                in_gateway_block = false;
            }
            if in_gateway_block && line.trim_start().starts_with("sha256:") {
                let indent: String = line
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .collect();
                rewritten.push_str(&format!("{indent}sha256: \"{current_build_sha}\"\n"));
            } else {
                rewritten.push_str(line);
                rewritten.push('\n');
            }
        }
        // Trim trailing whitespace introduced by the rewrite.
        let rewritten = rewritten.trim_end().to_string() + "\n";
        lock_content = rewritten.into_bytes();
        // Materialize the populated lock back to the deployed agent dir so
        // the on-disk file matches the current gateway build SHA. Without
        // this, the deployed runtime.lock keeps either the `replace-me`
        // placeholder (template case) or a stale real SHA (upgrade case),
        // and `check_runtime_lock_drift` (R+7/R+18) trips on the first
        // session turn. The revision's runtime.lock is already correct
        // (built from the in-memory `lock_content`), so this only fixes
        // the deployed copy.
        std::fs::write(&lock_path, &lock_content)?;
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

    // JavaScript agents run on the wasm tier: compile the `.js` entry to a
    // self-contained `.wasm` module at bundle time (Javy), bundle it, and
    // repoint the manifest's `script_entry` at it. The compiled module is
    // content-addressed in the revision like any other bundled file, so the
    // runtime executes it unchanged. Runs after the preset merge so the
    // `script_entry` rewrite lands on the final bundled SKILL.md.
    if matches!(parsed_manifest.execution_mode, ExecutionMode::Script) {
        if let Some(entry) = parsed_manifest.script_entry.clone() {
            if is_javascript_entry(&entry) {
                // Keep the entry strictly under the agent dir: it's used both to
                // read the source (`agent_dir.join(entry)`) and to name the
                // bundled output, which is later materialized under revision_dir.
                let entry_path = std::path::Path::new(&entry);
                anyhow::ensure!(
                    entry_path.is_relative()
                        && !entry_path
                            .components()
                            .any(|c| matches!(c, std::path::Component::ParentDir)),
                    "Agent '{}' script_entry '{}' must be a relative path within the agent dir (no `..`).",
                    agent_id,
                    entry
                );
                anyhow::ensure!(
                    parsed_manifest.runtime.sandbox == "wasm",
                    "Agent '{}' declares a JavaScript entry '{}' but sandbox '{}': \
                     JavaScript agents run on the wasm tier — set `sandbox: wasm`.",
                    agent_id,
                    entry,
                    parsed_manifest.runtime.sandbox
                );
                anyhow::ensure!(
                    crate::host_capabilities::is_javy_available(),
                    "Agent '{}' needs the Javy compiler (`javy`) on PATH to build JavaScript \
                     entry '{}' into wasm. Install Javy, then re-run; `autonoetic gateway \
                     preflight` shows toolchain availability.",
                    agent_id,
                    entry
                );
                let wasm_bytes = compile_js_to_wasm(&agent_dir.join(&entry)).map_err(|e| {
                    anyhow::anyhow!(
                        "Compiling JavaScript entry '{}' for agent '{}': {}",
                        entry,
                        agent_id,
                        e
                    )
                })?;
                let wasm_entry = wasm_entry_for(&entry);
                file_map.insert(wasm_entry.clone(), wasm_bytes);

                let skill_now = file_map
                    .get("SKILL.md")
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_else(|| skill_text.to_string());
                let rewritten =
                    set_script_entry_in_skill(&skill_now, &wasm_entry).ok_or_else(|| {
                        anyhow::anyhow!(
                            "Could not repoint script_entry to '{}' in SKILL.md for agent '{}'",
                            wasm_entry,
                            agent_id
                        )
                    })?;
                file_map.insert("SKILL.md".to_string(), rewritten.into_bytes());
                // Keep the in-memory manifest in sync for the execute-bit step
                // and the stored revision metadata below.
                parsed_manifest.script_entry = Some(wasm_entry);
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
        created_by_type: autonoetic_types::principal::PrincipalKind::Script.tag().to_string(),
        created_by_id: "cli".to_string(),
        source_kind: "bootstrap".to_string(),
        source_ref: None,
        origin_node_id: config.node_id.clone(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Candidate,
        metadata_json: serde_json::json!({
            "summary": "Bootstrapped from reference agent bundle",
            "manifest": {
                "description": parsed_manifest.agent.description,
                "capabilities": parsed_manifest.capabilities.iter().map(crate::runtime::tools::capability_type_name).collect::<Vec<_>>(),
                "execution_mode": match parsed_manifest.execution_mode {
                    ExecutionMode::Reasoning => "reasoning",
                    ExecutionMode::Script => "script",
                },
                "script_input_mode": serde_json::to_value(&parsed_manifest.script_input_mode).ok(),
                "script_entry": parsed_manifest.script_entry,
                "io": parsed_manifest.io,
            },
        }),
        short_id: String::new(),
        detected_network_hosts: None,
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
        None,
    )?;

    update_latest_symlink(gateway_dir, agent_id, &revision_id);

    Ok(true)
}

pub fn update_latest_symlink(gateway_dir: &Path, agent_id: &str, revision_id: &str) {
    let agent_rev_dir = gateway_dir.join("revisions").join("agents").join(agent_id);
    let latest_link = agent_rev_dir.join("latest");
    let _ = std::fs::remove_file(&latest_link);
    let _ = std::os::unix::fs::symlink(revision_id, &latest_link);
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
/// `model`, and `temperature` from the preset (when set). Always overrides
/// `thinking` when the preset defines it. Fills missing `base_url`, `chat_only`,
/// and `api_key_env` only if not already present in the agent's config.
/// Returns `None` if no modification was needed or if the frontmatter couldn't
/// be parsed.
/// Whether a script entry is JavaScript (compiled to wasm via Javy at bootstrap).
fn is_javascript_entry(entry: &str) -> bool {
    let lower = entry.to_ascii_lowercase();
    lower.ends_with(".js") || lower.ends_with(".mjs")
}

/// The bundled wasm entry name for a JavaScript source entry (`main.js` →
/// `main.wasm`, preserving any subdirectory).
fn wasm_entry_for(js_entry: &str) -> String {
    std::path::Path::new(js_entry)
        .with_extension("wasm")
        .to_string_lossy()
        .into_owned()
}

/// Compile a JavaScript file to a self-contained WASI module with Javy.
///
/// Uses `-C deterministic=y` (fixed clocks, zero-filled RNG during
/// pre-initialization) so identical source yields an identical module — the
/// compiled `.wasm` is content-addressed in the revision, so determinism keeps
/// rebuilds from churning the revision digest. The default (non-`dynamic`) build
/// embeds the QuickJS runtime, producing a standalone `_start` module the wasm
/// tier runs without a separate plugin.
fn compile_js_to_wasm(js_path: &Path) -> Result<Vec<u8>> {
    let out = tempfile::Builder::new().suffix(".wasm").tempfile()?;
    let result = std::process::Command::new("javy")
        .arg("build")
        .arg(js_path)
        .arg("-o")
        .arg(out.path())
        .arg("-C")
        .arg("deterministic=y")
        .output()
        .map_err(|e| anyhow::anyhow!("spawning javy: {e}"))?;
    anyhow::ensure!(
        result.status.success(),
        "javy build failed ({}): {}",
        result.status,
        String::from_utf8_lossy(&result.stderr).trim()
    );
    Ok(std::fs::read(out.path())?)
}

/// Repoint `script_entry` in a SKILL.md's YAML frontmatter to `new_entry`,
/// returning the rewritten file. Handles both SKILL.md shapes the parser accepts:
/// the Autonoetic-native format (top-level `script_entry`) and the AgentSkills
/// format (`metadata.autonoetic.script_entry`) — preferring top-level, matching
/// the parser's native-first precedence. Returns `None` if there's no
/// frontmatter or no place to write `script_entry`.
fn set_script_entry_in_skill(skill_text: &str, new_entry: &str) -> Option<String> {
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
    let key = serde_yaml::Value::String("script_entry".to_string());
    let value = serde_yaml::Value::String(new_entry.to_string());

    let top = yaml.as_mapping_mut()?;
    if top.contains_key(&key) {
        // Autonoetic-native: top-level script_entry.
        top.insert(key, value);
    } else if let Some(autonoetic) = top
        .get_mut(serde_yaml::Value::String("metadata".to_string()))
        .and_then(|m| m.get_mut(serde_yaml::Value::String("autonoetic".to_string())))
        .and_then(|a| a.as_mapping_mut())
    {
        // AgentSkills format: metadata.autonoetic.script_entry.
        autonoetic.insert(key, value);
    } else {
        return None;
    }

    let new_frontmatter = serde_yaml::to_string(&yaml).ok()?;
    let body = &skill_text[fm_end + 4..];
    Some(format!("---\n{}---{}\n", new_frontmatter, body))
}

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

            // base_url: always override (infrastructure-level, like provider/model)
            if let Some(ref base_url) = preset.base_url {
                let base_url_key = yaml_str("base_url");
                map.insert(base_url_key, serde_yaml::to_value(base_url).ok()?);
                modified = true;
            }

            // Fill-missing for these (agent-specific or optional)
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
            if let Some(ref thinking) = preset.thinking {
                map.insert(thinking_key, serde_yaml::to_value(thinking).ok()?);
                modified = true;
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

/// Materialize the Python (and optionally TypeScript) SDK into `.gateway/sdk/`
/// so the runtime has a self-contained copy independent of the source tree.
///
/// Skips silently when the source SDK is not found (e.g. deployed binary
/// without a source tree) — the env-var fallback in `resolve_python_sdk_path()`
/// can still point to a custom location.
pub fn bootstrap_sdk_snapshot(gateway_dir: &Path) -> Result<()> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sdk_root = manifest_dir.parent().map(|p| p.join("autonoetic-sdk"));

    let Some(sdk_root) = sdk_root else { return Ok(()) };
    if !sdk_root.exists() {
        return Ok(());
    }

    let dest = gateway_dir.join("sdk");

    // Python SDK
    let py_src = sdk_root.join("python").join("autonoetic_sdk");
    if py_src.exists() {
        let py_dest = dest.join("python").join("autonoetic_sdk");
        sync_dir(&py_src, &py_dest)?;
    }

    // TypeScript SDK
    let ts_src = sdk_root.join("typescript");
    if ts_src.exists() {
        let ts_dest = dest.join("typescript");
        sync_dir(&ts_src, &ts_dest)?;
    }

    Ok(())
}

/// Recursively copy files from `src` to `dst`. Overwrites when the destination
/// file differs in size or mtime. Used for the SDK snapshot — on upgrade the
/// SDK must be refreshed so the gateway can always find the latest bindings.
fn sync_dir(src: &Path, dst: &Path) -> Result<()> {
    if !dst.exists() {
        std::fs::create_dir_all(dst)?;
    }
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            sync_dir(&entry.path(), &dest_path)?;
        } else if file_type.is_file() {
            let src_meta = entry.metadata()?;
            let should_copy = match dest_path.metadata() {
                Ok(dst_meta) => {
                    dst_meta.len() != src_meta.len()
                        || dst_meta.modified().ok() != src_meta.modified().ok()
                }
                Err(_) => true,
            };
            if should_copy {
                std::fs::copy(&entry.path(), &dest_path)?;
            }
        }
    }
    Ok(())
}

/// Materialize the wiki docs corpus into `.gateway/wiki/` so agents can
/// discover and read platform documentation at runtime.
///
/// Only seeds pages that don't already exist — operator-promoted pages are
/// never overwritten on restart.
pub fn bootstrap_wiki_snapshot(gateway_dir: &Path) -> Result<()> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let wiki_src = manifest_dir
        .parent()
        .map(|p| p.join("docs").join("wiki"));

    let Some(wiki_src) = wiki_src else { return Ok(()) };
    if !wiki_src.exists() {
        return Ok(());
    }

    let dest = gateway_dir.join("wiki");
    seed_missing(&wiki_src, &dest)?;

    // Merge new built-in index entries into the existing index so new pages
    // shipped in binary upgrades become discoverable without overwriting
    // operator-promoted entries.
    let src_index = wiki_src.join("index.toml");
    let dst_index = dest.join("index.toml");
    if src_index.exists() {
        merge_index_toml(&src_index, &dst_index)?;
    }

    Ok(())
}

/// Merge page entries from `src` index.toml into `dst` index.toml. Entries in
/// `dst` whose `id` matches an entry in `src` are kept as-is (operator edits
/// or promotions win). New entries from `src` that have no matching `id` in
/// `dst` are appended, ensuring built-in pages shipped in upgrades become
/// discoverable without destroying operator-promoted state.
fn merge_index_toml(src: &Path, dst: &Path) -> Result<()> {
    let src_content = std::fs::read_to_string(src)?;
    let src_val: toml::Value = src_content.parse()?;
    let src_pages = src_val
        .get("pages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut dst_pages: Vec<toml::Value> = if dst.exists() {
        let dst_content = std::fs::read_to_string(dst)?;
        match dst_content.parse::<toml::Value>() {
            Ok(v) => v.get("pages").and_then(|p| p.as_array().cloned()).unwrap_or_default(),
            Err(_) => {
                tracing::warn!(
                    target: "bootstrap",
                    "wiki index.toml parse error, rebuilding from source: {}",
                    dst.display(),
                );
                src_pages.clone()
            }
        }
    } else {
        src_pages.clone()
    };

    let existing_ids: std::collections::HashSet<String> = dst_pages
        .iter()
        .filter_map(|p| p.get("id").and_then(|v| v.as_str()).map(String::from))
        .collect();

    for entry in &src_pages {
        if let Some(id) = entry.get("id").and_then(|v| v.as_str()) {
            if !existing_ids.contains(id) {
                dst_pages.push(entry.clone());
            }
        }
    }

    let merged = toml::Value::Table(
        vec![("pages".to_string(), toml::Value::Array(dst_pages))]
            .into_iter()
            .collect(),
    );
    std::fs::write(dst, merged.to_string())?;
    Ok(())
}

/// Copy files from `src` to `dst` only when the destination file is missing.
/// Never overwrites existing files — operator-promoted wiki pages and index
/// entries survive restarts.
fn seed_missing(src: &Path, dst: &Path) -> Result<()> {
    if !dst.exists() {
        std::fs::create_dir_all(dst)?;
    }
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            seed_missing(&entry.path(), &dest_path)?;
        } else if file_type.is_file() && !dest_path.exists() {
            std::fs::copy(&entry.path(), &dest_path)?;
        }
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn javascript_entry_detection_and_wasm_naming() {
        assert!(is_javascript_entry("main.js"));
        assert!(is_javascript_entry("scripts/agent.MJS"));
        assert!(!is_javascript_entry("main.py"));
        assert!(!is_javascript_entry("main.wasm"));
        assert_eq!(wasm_entry_for("main.js"), "main.wasm");
        assert_eq!(wasm_entry_for("scripts/agent.mjs"), "scripts/agent.wasm");
    }

    fn reparse_frontmatter(out: &str) -> serde_yaml::Value {
        let fm = out.trim_start().strip_prefix("---").unwrap();
        let end = fm.find("\n---").unwrap();
        serde_yaml::from_str(&fm[..end]).unwrap()
    }

    #[test]
    fn set_script_entry_rewrites_agentskills_frontmatter() {
        // AgentSkills format: metadata.autonoetic.script_entry.
        let skill = "---\nmetadata:\n  autonoetic:\n    execution_mode: script\n    script_entry: main.js\n    runtime:\n      sandbox: wasm\n---\nbody text\n";
        let out = set_script_entry_in_skill(skill, "main.wasm").expect("should rewrite");
        let yaml = reparse_frontmatter(&out);
        assert_eq!(
            yaml["metadata"]["autonoetic"]["script_entry"].as_str(),
            Some("main.wasm")
        );
        assert!(out.contains("body text"));
    }

    #[test]
    fn set_script_entry_rewrites_native_top_level_frontmatter() {
        // Autonoetic-native format: top-level script_entry.
        let skill = "---\nexecution_mode: script\nscript_entry: main.js\nruntime:\n  sandbox: wasm\n---\nnative body\n";
        let out = set_script_entry_in_skill(skill, "main.wasm").expect("should rewrite");
        let yaml = reparse_frontmatter(&out);
        assert_eq!(yaml["script_entry"].as_str(), Some("main.wasm"));
        assert!(out.contains("native body"));
    }

    #[test]
    fn set_script_entry_returns_none_when_no_target() {
        // No top-level script_entry and no metadata.autonoetic mapping.
        assert!(set_script_entry_in_skill("---\nfoo: bar\n---\nx\n", "main.wasm").is_none());
        assert!(set_script_entry_in_skill("no frontmatter", "main.wasm").is_none());
    }

    #[test]
    fn compile_js_to_wasm_produces_a_wasm_module() {
        if !crate::host_capabilities::is_javy_available() {
            eprintln!("skipping: javy not on PATH");
            return;
        }
        let dir = tempdir().unwrap();
        let js = dir.path().join("main.js");
        std::fs::write(&js, "console.log('hi');").unwrap();
        let bytes = compile_js_to_wasm(&js).expect("javy build should succeed");
        assert_eq!(&bytes[0..4], b"\0asm", "output must be a wasm module");
    }

    fn minimal_skill(agent_id: &str) -> String {
        format!(
            "---\nversion: \"1.0\"\nruntime:\n  engine: \"autonoetic\"\n  gateway_version: \"0.1.0\"\n  sdk_version: \"0.1.0\"\n  type: \"stateful\"\n  sandbox: \"bubblewrap\"\n  runtime_lock: \"runtime.lock\"\nagent:\n  id: \"{agent_id}\"\n  name: \"{agent_id}\"\n  description: \"test\"\ncapabilities: []\n---\nbody\n"
        )
    }

    fn minimal_config(agents_dir: &std::path::Path) -> autonoetic_types::config::GatewayConfig {
        let mut config = autonoetic_types::config::GatewayConfig::default();
        config.agents_dir = agents_dir.to_path_buf();
        config
    }

    fn write_runtime_lock_yaml(agent_dir: &std::path::Path, sha_value: &str) {
        let lock = format!(
            "gateway:\n  artifact: \"marketplace://gateway/autonoetic-gateway\"\n  version: \"0.1.0\"\n  sha256: \"{sha_value}\"\nsdk:\n  version: \"0.1.0\"\nsandbox:\n  backend: \"bubblewrap\"\ndependencies: []\nartifacts: []\n"
        );
        std::fs::write(agent_dir.join("runtime.lock"), lock).unwrap();
    }

    fn bootstrap_test_agent(agent_id: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        let agent_dir = agents_dir.join(agent_id);
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("SKILL.md"), minimal_skill(agent_id)).unwrap();
        // Use the current build SHA as the initial lock so the first
        // bootstrap creates a revision. The individual tests then mutate
        // the lock to placeholder / stale and re-bootstrap to exercise
        // the rewrite path.
        let current_sha = crate::runtime::install_contract::GATEWAY_BUILD_SHA256;
        write_runtime_lock_yaml(&agent_dir, current_sha);
        let config = minimal_config(&agents_dir);
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        let _ = bootstrap_agent_inner(&config, &gateway_dir, &store, agent_id).unwrap();
        (dir, agent_dir)
    }

    /// Regression: when the on-disk runtime.lock has the `replace-me`
    /// placeholder, bootstrap must materialize the populated lock back to
    /// disk so `check_runtime_lock_drift` sees the current build SHA.
    #[test]
    fn bootstrap_materializes_placeholder_lock_on_disk() {
        let (_dir, agent_dir) = bootstrap_test_agent("test-agent");
        write_runtime_lock_yaml(&agent_dir, "replace-me");
        // Re-run bootstrap to trigger the rewrite path (the helper above
        // already ran once, but with whatever the file contained then —
        // here we set the placeholder explicitly and re-bootstrap).
        let agents_dir = agent_dir.parent().unwrap().to_path_buf();
        let config = minimal_config(&agents_dir);
        let gateway_dir = agents_dir.join(".gateway");
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        let _ = bootstrap_agent_inner(&config, &gateway_dir, &store, "test-agent").unwrap();
        let written = std::fs::read_to_string(agent_dir.join("runtime.lock")).unwrap();
        let current_sha = crate::runtime::install_contract::GATEWAY_BUILD_SHA256;
        assert!(
            !written.contains("replace-me"),
            "placeholder must be replaced, got:\n{written}"
        );
        assert!(
            written.contains(&format!("sha256: \"{current_sha}\"")),
            "deployed lock must carry current build SHA, got:\n{written}"
        );
    }

    /// Regression: when the on-disk lock has a real but STALE build SHA
    /// (the user upgraded the binary but the deployed lock wasn't refreshed),
    /// bootstrap must update it to the current build SHA.
    #[test]
    fn bootstrap_refreshes_stale_real_sha_on_disk() {
        let (_dir, agent_dir) = bootstrap_test_agent("test-agent");
        write_runtime_lock_yaml(&agent_dir, "sha256:stale_old_digest");
        let agents_dir = agent_dir.parent().unwrap().to_path_buf();
        let config = minimal_config(&agents_dir);
        let gateway_dir = agents_dir.join(".gateway");
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        let _ = bootstrap_agent_inner(&config, &gateway_dir, &store, "test-agent").unwrap();
        let written = std::fs::read_to_string(agent_dir.join("runtime.lock")).unwrap();
        let current_sha = crate::runtime::install_contract::GATEWAY_BUILD_SHA256;
        assert!(
            !written.contains("stale_old_digest"),
            "stale SHA must be replaced, got:\n{written}"
        );
        assert!(
            written.contains(&format!("sha256: \"{current_sha}\"")),
            "deployed lock must carry current build SHA, got:\n{written}"
        );
    }

    /// The on-disk lock should be left alone when its SHA already matches
    /// the current gateway build (idempotent re-bootstrap).
    #[test]
    fn bootstrap_does_not_touch_lock_when_sha_already_current() {
        let current_sha = crate::runtime::install_contract::GATEWAY_BUILD_SHA256;
        let original_lock = format!(
            "gateway:\n  artifact: \"marketplace://gateway/autonoetic-gateway\"\n  version: \"0.1.0\"\n  sha256: \"{current_sha}\"\nsdk:\n  version: \"0.1.0\"\nsandbox:\n  backend: \"bubblewrap\"\ndependencies: []\nartifacts: []\n"
        );
        let (_dir, agent_dir) = bootstrap_test_agent("test-agent");
        std::fs::write(agent_dir.join("runtime.lock"), &original_lock).unwrap();
        let agents_dir = agent_dir.parent().unwrap().to_path_buf();
        let config = minimal_config(&agents_dir);
        let gateway_dir = agents_dir.join(".gateway");
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        let _ = bootstrap_agent_inner(&config, &gateway_dir, &store, "test-agent").unwrap();
        let written = std::fs::read_to_string(agent_dir.join("runtime.lock")).unwrap();
        assert_eq!(
            written, original_lock,
            "lock with current SHA must be left untouched"
        );
    }
}
