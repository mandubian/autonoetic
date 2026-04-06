//! Bootstrap agents from the agents directory into the gateway store.
//!
//! Scans `config.agents_dir` for agent bundles (directories with `SKILL.md`),
//! creates revisions from their content, and auto-promotes them. Skips agents
//! that already have revisions.

use crate::scheduler::gateway_store::GatewayStore;
use anyhow::Result;
use autonoetic_types::agent_revision::{AgentRevisionRecord, AgentRevisionStatus};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::id_format::mint_hashed_prefixed_id;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

/// Bootstrap all agents from `config.agents_dir` into the gateway store.
/// Returns the number of agents activated.
pub fn bootstrap_agents(config: &GatewayConfig, gateway_dir: &Path) -> Result<usize> {
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

        // Skip if the agent already has revisions
        let existing = store.list_agent_revisions(&agent_id)?;
        if !existing.is_empty() {
            continue;
        }

        let skill_content = std::fs::read(&skill_path)?;
        let skill_text = String::from_utf8_lossy(&skill_content);
        let (parsed_manifest, _instructions) =
            crate::runtime::parser::SkillParser::parse(&skill_text).map_err(|e| {
                anyhow::anyhow!("Failed to parse SKILL.md for '{}': {}", agent_id, e)
            })?;

        let lock_rel_path = &parsed_manifest.runtime.runtime_lock;
        let lock_path = agent_dir.join(lock_rel_path);
        let lock_content = std::fs::read(&lock_path).map_err(|e| {
            anyhow::anyhow!(
                "Missing runtime.lock '{}' for agent '{}': {}",
                lock_rel_path,
                agent_id,
                e
            )
        })?;

        let manifest_hash = format!("sha256:{:x}", Sha256::digest(&skill_content));
        let runtime_lock_hash = format!("sha256:{:x}", Sha256::digest(&lock_content));

        let mut file_map: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        collect_files(&agent_dir, &agent_dir, &mut file_map)?;
        file_map.insert(lock_rel_path.clone(), lock_content.clone());

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
            continue;
        }

        let revision_dir = gateway_dir
            .join("revisions")
            .join("agents")
            .join(&agent_id)
            .join(&revision_id);

        if !revision_dir.exists() {
            for (rel_path, bytes) in &file_map {
                let dest = revision_dir.join(rel_path);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&dest, bytes)?;
            }
        }

        let now = chrono::Utc::now().to_rfc3339();
        let rev = AgentRevisionRecord {
            revision_id: revision_id.clone(),
            agent_id: agent_id.clone(),
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
        };

        store.insert_agent_revision_transactional(&rev)?;

        let promotion_id =
            mint_hashed_prefixed_id("prom-", &format!("{}-{}-{}", agent_id, revision_id, now));

        store.atomic_promote(
            &agent_id,
            &revision_id,
            &promotion_id,
            "bootstrap",
            "cli",
            Some("Auto-promoted during agent bootstrap"),
            None,
        )?;

        activated += 1;
    }

    Ok(activated)
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
