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

#[derive(Debug, Clone)]
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
    /// Conflict policy for memory entries that already exist locally.
    pub memory_conflict_policy: MemoryConflictPolicy,
}

impl Default for ImportRequest {
    fn default() -> Self {
        Self {
            archive_path: PathBuf::new(),
            verify_signature: false,
            dry_run: false,
            activate: false,
            trust_domain_override: None,
            memory_conflict_policy: MemoryConflictPolicy::default(),
        }
    }
}

/// What to do when the incoming capsule carries a memory entry whose
/// `memory_id` already exists on the receiving gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryConflictPolicy {
    /// Keep the local copy; skip the imported one (default — safest).
    KeepLocal,
    /// Overwrite the local copy with the imported one.
    OverwriteLocal,
}

impl Default for MemoryConflictPolicy {
    fn default() -> Self {
        MemoryConflictPolicy::KeepLocal
    }
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
    /// Number of memory entries actually persisted (post-conflict-policy).
    pub memory_entries_imported: usize,
    /// Number of memory entries skipped because a local copy existed and
    /// the policy was `KeepLocal`.
    pub memory_entries_skipped: usize,
    /// Number of scheduled jobs recreated on this gateway (Headless mode).
    pub scheduled_jobs_recreated: usize,
    /// True when a session checkpoint from the capsule was restored
    /// into the gateway's checkpoint store (Replay mode).
    pub checkpoint_restored: bool,
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

    // Platform compatibility: refuse cross-platform layer imports when
    // the trust domain is anything other than `"local"`. Within the same
    // trust boundary we trust the operator's judgment (mostly so dev
    // workflows on macOS hosts can pull capsules built in Linux CI).
    if trust_domain != "local" {
        if let Some(p) = &manifest.platform {
            let local_os = std::env::consts::OS;
            let local_arch = std::env::consts::ARCH;
            if p.os != local_os || p.arch != local_arch {
                anyhow::bail!(
                    "capsule was built for {}/{} but this gateway is {}/{}; \
                     refusing import in trust_domain={} (override --trust-domain local to bypass)",
                    p.os,
                    p.arch,
                    local_os,
                    local_arch,
                    trust_domain
                );
            }
        }
    }

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
            memory_entries_imported: 0,
            memory_entries_skipped: 0,
            scheduled_jobs_recreated: 0,
            checkpoint_restored: false,
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
            created_by_type: autonoetic_types::principal::PrincipalKind::Script.tag().to_string(),
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

    let (memory_imported, memory_skipped) = import_memory_snapshot(
        ctx.gateway_store,
        extract.path(),
        manifest.memory_snapshot.as_ref(),
        req.memory_conflict_policy,
        &manifest.agent_id,
    )?;

    let scheduled_jobs_recreated = if matches!(manifest.mode, autonoetic_types::capsule::CapsuleMode::Headless) {
        recreate_scheduled_jobs(ctx.gateway_store, &manifest.scheduled_jobs)?
    } else {
        0
    };

    let checkpoint_restored = if matches!(manifest.mode, autonoetic_types::capsule::CapsuleMode::Replay) {
        restore_checkpoint(ctx.gateway_config, extract.path(), &manifest)?
    } else {
        false
    };

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
        memory_entries_imported: memory_imported,
        memory_entries_skipped: memory_skipped,
        scheduled_jobs_recreated,
        checkpoint_restored,
    })
}

/// Reject manifest-supplied relative paths that could read outside the
/// extracted capsule directory. Same rules as the tar entry guard:
/// non-empty, no absolute prefix, no `..` segments, no Windows-style
/// drive letters or backslashes.
fn validate_archive_relative_path(p: &str, label: &str) -> Result<()> {
    if p.is_empty() {
        anyhow::bail!("capsule manifest has empty {}", label);
    }
    if p.starts_with('/') || p.contains('\\') {
        anyhow::bail!(
            "capsule manifest {} {:?} is absolute or contains backslashes",
            label,
            p
        );
    }
    let path = std::path::Path::new(p);
    for component in path.components() {
        use std::path::Component;
        match component {
            Component::Prefix(_) | Component::RootDir => {
                anyhow::bail!(
                    "capsule manifest {} {:?} contains an absolute prefix",
                    label,
                    p
                );
            }
            Component::ParentDir => {
                anyhow::bail!(
                    "capsule manifest {} {:?} contains parent-dir segment",
                    label,
                    p
                );
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(())
}

fn import_memory_snapshot(
    store: &Arc<GatewayStore>,
    extract_root: &Path,
    snapshot: Option<&autonoetic_types::capsule::CapsuleMemorySnapshot>,
    policy: MemoryConflictPolicy,
    expected_agent_id: &str,
) -> Result<(usize, usize)> {
    let Some(snapshot) = snapshot else {
        return Ok((0, 0));
    };
    // The handle comes from the (potentially untrusted) manifest. Refuse
    // anything that could escape the extracted capsule directory.
    validate_archive_relative_path(&snapshot.content_handle, "memory content_handle")?;
    let path = extract_root.join(&snapshot.content_handle);
    if !path.is_file() {
        return Ok((0, 0));
    }
    let bytes = std::fs::read(&path)?;
    let parsed: serde_json::Value = serde_json::from_slice(&bytes)?;
    let entries = match parsed.get("entries").and_then(|v| v.as_array()) {
        Some(arr) => arr.clone(),
        None => return Ok((0, 0)),
    };
    let mut imported = 0usize;
    let mut skipped = 0usize;
    for entry in entries {
        let obj: autonoetic_types::memory::MemoryObject = match serde_json::from_value(entry) {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(
                    target: "capsule",
                    error = %e,
                    "skipping malformed memory entry"
                );
                continue;
            }
        };
        // Refuse memory entries that claim a different owner — a
        // tampered/unsigned capsule must not be able to inject
        // arbitrary memories for unrelated agents into the receiver.
        if obj.owner_agent_id != expected_agent_id {
            tracing::warn!(
                target: "capsule",
                memory_id = %obj.memory_id,
                claimed_owner = %obj.owner_agent_id,
                expected_owner = %expected_agent_id,
                "skipping memory entry whose owner_agent_id does not match the capsule's agent_id"
            );
            skipped += 1;
            continue;
        }
        let existing = store.memory_get_unrestricted(&obj.memory_id)?;
        match (existing, policy) {
            (Some(_), MemoryConflictPolicy::KeepLocal) => skipped += 1,
            (_, _) => {
                store.memory_upsert(&obj)?;
                imported += 1;
            }
        }
    }
    Ok((imported, skipped))
}

fn recreate_scheduled_jobs(
    store: &Arc<GatewayStore>,
    jobs: &[autonoetic_types::capsule::CapsuleScheduledJob],
) -> Result<usize> {
    let mut created = 0;
    for j in jobs {
        let now = Utc::now().to_rfc3339();
        let new_id = format!("job_capsule_{}_{}", j.job_id, uuid::Uuid::new_v4());
        let job = autonoetic_types::scheduled_job::ScheduledJob {
            job_id: new_id,
            owner_agent_id: j.owner_agent_id.clone(),
            root_session_id: j.root_session_id.clone(),
            target_agent_id: j.target_agent_id.clone(),
            target_revision_id: j.target_revision_id.clone(),
            message: j.message.clone(),
            metadata_json: j.metadata_json.clone(),
            cron_expr: j.cron_expr.clone(),
            timezone: j.timezone.clone(),
            next_run_at: now.clone(),
            last_run_at: None,
            status: autonoetic_types::scheduled_job::ScheduledJobStatus::Active,
            created_at: now.clone(),
            updated_at: now,
            last_error: None,
            generation: 0,
        };
        match store.create_scheduled_job(&job) {
            Ok(()) => created += 1,
            Err(e) => {
                tracing::warn!(
                    target: "capsule",
                    error = %e,
                    job_id = %j.job_id,
                    "failed to re-create scheduled job; continuing"
                );
            }
        }
    }
    Ok(created)
}

fn restore_checkpoint(
    config: &GatewayConfig,
    extract_root: &Path,
    manifest: &CapsuleManifest,
) -> Result<bool> {
    let Some(rel) = &manifest.checkpoint_handle else {
        return Ok(false);
    };
    // The checkpoint path is operator-trusted only when signed; even
    // then, guard against absolute / parent-dir segments before joining.
    validate_archive_relative_path(rel, "checkpoint_handle")?;
    let path = extract_root.join(rel);
    if !path.is_file() {
        return Ok(false);
    }
    let bytes = std::fs::read(&path)?;
    let ckpt: crate::runtime::checkpoint::SessionCheckpoint = serde_json::from_slice(&bytes)?;
    // A tampered/unsigned capsule could otherwise inject a checkpoint
    // for an unrelated agent into the receiver's checkpoint store.
    if ckpt.agent_id != manifest.agent_id {
        anyhow::bail!(
            "Replay-mode checkpoint agent_id {:?} does not match manifest agent_id {:?}",
            ckpt.agent_id,
            manifest.agent_id
        );
    }
    crate::runtime::checkpoint::save_checkpoint(config, &ckpt)?;
    Ok(true)
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
        updated_by_type: autonoetic_types::principal::PrincipalKind::Script.tag().to_string(),
        updated_by_id: "capsule.import".to_string(),
        reason: Some("capsule import --activate".to_string()),
        suspended_at: None,
        suspended_reason: None,
        suspended_by: None,
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
