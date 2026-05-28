//! Capsule import pipeline.
//!
//! Extracts a `tar.zst` capsule into a tempdir, parses `capsule.json`,
//! verifies the signature against the configured trust store, materialises
//! the revision directory under `.gateway/revisions/agents/<agent>/<rev>/`,
//! inserts a fresh `AgentRevisionRecord` with
//! `source_kind = "capsule_import"`, and emits a `capsule.import` causal
//! event.
//!
//! Phase 2 ships the thin-mode happy path end-to-end. Hermetic-layer
//! dedup, memory merge with conflict policy, and replay-mode session
//! resume layer on top in Phase 4.

use anyhow::{Context, Result};
use autonoetic_types::capsule::CapsuleManifest;
use autonoetic_types::config::GatewayConfig;
use chrono::Utc;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::capsule::archive;
use crate::capsule::verify::{verify_signature, SignatureStatus};
use crate::scheduler::gateway_store::GatewayStore;

const CAPSULE_FORMAT_MAJOR_SUPPORTED: u64 = 1;

#[derive(Debug, Clone, Default)]
pub struct ImportRequest {
    pub archive_path: PathBuf,
    /// Require a present + verified signature.
    pub verify_signature: bool,
    /// Run validation only; do not persist anything.
    pub dry_run: bool,
    /// When true, after importing the revision rebind the alias.
    pub activate: bool,
    /// Override the trust domain stamped on the imported revision.
    /// Defaults to `"local"`.
    pub trust_domain_override: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ImportOutcome {
    pub capsule_id: String,
    pub agent_id: String,
    pub revision_id: String,
    pub revision_short_id: String,
    pub signature_status: String,
    pub dry_run: bool,
    pub dedup_savings_bytes: u64,
    pub created_revision: bool,
}

pub struct ImportContext<'a> {
    pub gateway_dir: &'a Path,
    pub gateway_config: &'a GatewayConfig,
    pub gateway_store: &'a Arc<GatewayStore>,
}

pub fn import(req: ImportRequest, ctx: ImportContext<'_>) -> Result<ImportOutcome> {
    let cfg = &ctx.gateway_config.capsule;
    let archive_size = std::fs::metadata(&req.archive_path)
        .with_context(|| format!("stat capsule archive {}", req.archive_path.display()))?
        .len();
    if archive_size > cfg.max_capsule_size_bytes {
        anyhow::bail!(
            "capsule archive size {} exceeds configured max {}",
            archive_size,
            cfg.max_capsule_size_bytes
        );
    }

    let extract = tempfile::tempdir().context("creating capsule extract tempdir")?;
    archive::unpack(&req.archive_path, extract.path(), cfg.max_capsule_size_bytes)?;

    let manifest_bytes = archive::read_entry(extract.path(), "capsule.json")
        .context("reading capsule.json from extracted archive")?;
    let manifest: CapsuleManifest = serde_json::from_slice(&manifest_bytes)
        .context("parsing capsule.json")?;

    if let Some(major) = manifest.format_major_version() {
        if major > CAPSULE_FORMAT_MAJOR_SUPPORTED {
            anyhow::bail!(
                "unsupported capsule format_version {} (this gateway supports {}.x)",
                manifest.format_version,
                CAPSULE_FORMAT_MAJOR_SUPPORTED
            );
        }
    } else {
        anyhow::bail!(
            "capsule.json has malformed format_version: {}",
            manifest.format_version
        );
    }

    let sig_status = verify_signature(&manifest, cfg, req.verify_signature)?;
    if req.verify_signature && !sig_status.is_ok() {
        anyhow::bail!("signature verification failed: {:?}", sig_status);
    }

    let revision_id = manifest.revision_id.clone();
    let agent_id = manifest.agent_id.clone();
    // Both fields are interpolated into filesystem paths under
    // `.gateway/revisions/agents/<agent_id>/<revision_id>/`. They
    // come from an untrusted capsule manifest, so refuse anything
    // that could break out of that directory or include directory
    // separators.
    validate_path_component(&agent_id, "agent_id")?;
    validate_path_component(&revision_id, "revision_id")?;
    let trust_domain = req
        .trust_domain_override
        .clone()
        .unwrap_or_else(|| "local".to_string());

    let agent_dir = extract.path().join("agent");
    let file_map = read_agent_files(&agent_dir)?;
    let (dedup_savings, _content_handles) =
        stage_blobs_into_content_store(ctx.gateway_dir, &file_map)?;

    if req.dry_run {
        return Ok(ImportOutcome {
            capsule_id: manifest.capsule_id,
            agent_id,
            revision_id,
            revision_short_id: manifest.revision_short_id,
            signature_status: format!("{:?}", sig_status),
            dry_run: true,
            dedup_savings_bytes: dedup_savings,
            created_revision: false,
        });
    }

    let revision_existed = ctx
        .gateway_store
        .get_agent_revision(&revision_id)?
        .is_some();

    if !revision_existed {
        materialize_revision_dir(ctx.gateway_dir, &agent_id, &revision_id, &file_map)?;
        let now = Utc::now().to_rfc3339();
        let runtime_lock_hash = file_map
            .get("runtime.lock")
            .map(|bytes| format!("sha256:{}", sha256_hex(bytes)))
            .unwrap_or_else(|| String::new());
        let manifest_hash = file_map
            .get("SKILL.md")
            .map(|bytes| format!("sha256:{}", sha256_hex(bytes)))
            .unwrap_or_else(|| String::new());

        let rev = autonoetic_types::agent_revision::AgentRevisionRecord {
            revision_id: revision_id.clone(),
            agent_id: agent_id.clone(),
            base_revision_id: None,
            artifact_id: None,
            content_digest: manifest.content_digest.clone(),
            runtime_lock_hash,
            manifest_hash,
            created_at: now,
            created_by_type: "system".to_string(),
            created_by_id: "capsule.import".to_string(),
            source_kind: "capsule_import".to_string(),
            source_ref: Some(manifest.capsule_id.clone()),
            origin_node_id: manifest.provenance.origin_node_id.clone(),
            trust_domain: trust_domain.clone(),
            status: autonoetic_types::agent_revision::AgentRevisionStatus::Candidate,
            metadata_json: serde_json::json!({
                "capsule": {
                    "capsule_id": manifest.capsule_id,
                    "format_version": manifest.format_version,
                    "mode": manifest.mode,
                    "provenance": manifest.provenance,
                    "signature_status": format!("{:?}", sig_status),
                    "redactions": manifest.redactions,
                }
            }),
            short_id: manifest.revision_short_id.clone(),
            signature: manifest.signature.as_ref().map(|s| s.signature.clone()),
            signer_id: manifest.signature.as_ref().map(|s| s.signer_id.clone()),
        };
        ctx.gateway_store.insert_agent_revision(&rev)?;
    }

    if req.activate {
        bind_alias(ctx.gateway_store, &agent_id, &revision_id)?;
    }

    emit_import_event(
        ctx.gateway_store,
        &manifest.capsule_id,
        &agent_id,
        &revision_id,
        dedup_savings,
        &sig_status,
    )?;

    Ok(ImportOutcome {
        capsule_id: manifest.capsule_id,
        agent_id,
        revision_id,
        revision_short_id: manifest.revision_short_id,
        signature_status: format!("{:?}", sig_status),
        dry_run: false,
        dedup_savings_bytes: dedup_savings,
        created_revision: !revision_existed,
    })
}

fn read_agent_files(agent_dir: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut map = BTreeMap::new();
    if !agent_dir.exists() {
        return Ok(map);
    }
    walk(agent_dir, agent_dir, &mut map)?;
    Ok(map)
}

fn walk(
    base: &Path,
    cur: &Path,
    map: &mut BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    for entry in std::fs::read_dir(cur)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if file_type.is_dir() {
            walk(base, &path, map)?;
        } else if file_type.is_file() {
            map.insert(rel, std::fs::read(&path)?);
        }
    }
    Ok(())
}

fn stage_blobs_into_content_store(
    gateway_dir: &Path,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<(u64, Vec<String>)> {
    let store = crate::runtime::content_store::ContentStore::new(gateway_dir)?;
    let mut dedup_savings: u64 = 0;
    let mut handles = Vec::new();
    for bytes in files.values() {
        let handle = crate::runtime::content_store::ContentStore::compute_handle(bytes);
        if store.exists(&handle) {
            dedup_savings = dedup_savings.saturating_add(bytes.len() as u64);
        }
        store.write(bytes)?;
        handles.push(handle);
    }
    Ok((dedup_savings, handles))
}

fn materialize_revision_dir(
    gateway_dir: &Path,
    agent_id: &str,
    revision_id: &str,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<PathBuf> {
    let rev_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join(agent_id)
        .join(revision_id);
    if rev_dir.exists() {
        return Ok(rev_dir);
    }
    if let Some(parent) = rev_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = rev_dir
        .parent()
        .unwrap()
        .join(format!(".tmp-{}-{}", revision_id, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp)?;
    for (rel, bytes) in files {
        let dst = tmp.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dst, bytes)?;
    }
    match std::fs::rename(&tmp, &rev_dir) {
        Ok(()) => Ok(rev_dir),
        Err(_) if rev_dir.exists() => {
            let _ = std::fs::remove_dir_all(&tmp);
            Ok(rev_dir)
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            Err(e.into())
        }
    }
}

fn bind_alias(
    store: &Arc<GatewayStore>,
    agent_id: &str,
    revision_id: &str,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let alias = autonoetic_types::agent_revision::AgentAliasRecord {
        alias_id: agent_id.to_string(),
        agent_id: agent_id.to_string(),
        revision_id: revision_id.to_string(),
        updated_at: now,
        updated_by_type: "system".to_string(),
        updated_by_id: "capsule.import".to_string(),
        reason: Some("capsule import --activate".to_string()),
    };
    store.upsert_agent_alias(&alias)?;
    Ok(())
}

fn emit_import_event(
    store: &Arc<GatewayStore>,
    capsule_id: &str,
    agent_id: &str,
    revision_id: &str,
    dedup_savings: u64,
    sig_status: &SignatureStatus,
) -> Result<()> {
    let payload = serde_json::json!({
        "capsule_id": capsule_id,
        "revision_id": revision_id,
        "dedup_savings_bytes": dedup_savings,
        "signature_status": format!("{:?}", sig_status),
    });
    let event = autonoetic_types::causal_chain::CausalEventRecord {
        event_id: uuid::Uuid::new_v4().to_string(),
        agent_id: agent_id.to_string(),
        session_id: "gateway".to_string(),
        turn_id: None,
        event_seq: 0,
        timestamp: Utc::now().to_rfc3339(),
        category: "capsule".to_string(),
        action: "import".to_string(),
        status: "SUCCESS".to_string(),
        enforced_rules: autonoetic_types::causal_chain::default_enforced_rules(),
        target: Some(format!("{}@{}", agent_id, revision_id)),
        payload: Some(payload.to_string()),
        payload_ref: None,
        evidence_ref: None,
        reason: None,
    };
    store.create_causal_event(&event)?;
    Ok(())
}

/// Refuse manifest-supplied strings that would let an attacker steer
/// the import pipeline outside the per-agent / per-revision directory
/// or assemble unexpected paths. Allowed characters are intentionally
/// narrow (alphanumeric, `_`, `-`, `.`, and `:` for the
/// `rev_sha256:abcd…` convention); the value also must not be empty
/// or contain `..` segments.
fn validate_path_component(value: &str, label: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        anyhow::bail!("capsule manifest has empty {}", label);
    }
    if value.contains("..") {
        anyhow::bail!(
            "capsule manifest {} {:?} contains parent-dir segment",
            label,
            value
        );
    }
    if value.contains('/') || value.contains('\\') {
        anyhow::bail!(
            "capsule manifest {} {:?} contains a path separator",
            label,
            value
        );
    }
    for ch in value.chars() {
        let ok = ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':');
        if !ok {
            anyhow::bail!(
                "capsule manifest {} {:?} contains unsupported character {:?}",
                label,
                value,
                ch
            );
        }
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_collects_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("a.txt"), b"A").unwrap();
        std::fs::write(dir.path().join("nested/b.txt"), b"B").unwrap();
        let mut map = BTreeMap::new();
        walk(dir.path(), dir.path(), &mut map).unwrap();
        assert_eq!(map.get("a.txt"), Some(&b"A".to_vec()));
        assert_eq!(map.get("nested/b.txt"), Some(&b"B".to_vec()));
    }

    #[test]
    fn sha256_hex_is_stable() {
        assert_eq!(
            sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn validate_path_component_accepts_canonical_ids() {
        assert!(validate_path_component("planner.default", "agent_id").is_ok());
        assert!(
            validate_path_component("rev_sha256:abcdef1234567890", "revision_id").is_ok()
        );
    }

    #[test]
    fn validate_path_component_rejects_traversal_and_separators() {
        for bad in ["", "..", "../etc", "foo/bar", "foo\\bar", "foo bar", "foo;bar"] {
            assert!(
                validate_path_component(bad, "agent_id").is_err(),
                "{:?} should be rejected",
                bad
            );
        }
    }
}
