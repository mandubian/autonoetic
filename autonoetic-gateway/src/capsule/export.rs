//! Capsule export pipeline.
//!
//! Given an `AgentRevisionRecord` and a target archive path, stages the
//! revision's files into a tempdir, applies redaction, builds the
//! [`CapsuleManifest`], optionally signs it, packs as `tar.zst`, and
//! emits a `capsule.export` causal event.
//!
//! Hermetic / Replay / Headless modes layer additional content
//! (embedded layers, session checkpoint, scheduled jobs) on top of the
//! shared thin-mode body. Phases 2 ships the thin and hermetic paths
//! end-to-end; replay/headless leave the right hooks (`checkpoint_handle`,
//! scheduled-job collection) for Phase 4 to fill in.

use anyhow::{Context, Result};
use autonoetic_types::capsule::{
    CapsuleManifest, CapsuleMode, CapsuleProvenance, CAPSULE_FORMAT_VERSION,
};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::redaction::{redact_embedded_secrets, redact_json_value};
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::capsule::archive;
use crate::capsule::verify;
use crate::runtime::crypto::GatewayIdentityKey;
use crate::scheduler::gateway_store::GatewayStore;

/// Inputs to the export pipeline.
#[derive(Debug, Clone)]
pub struct ExportRequest {
    pub agent_id: String,
    /// Specific revision selector. `None` resolves the current alias.
    pub revision_id: Option<String>,
    pub mode: CapsuleMode,
    /// When `true`, include a redacted memory snapshot. If unset, falls
    /// back to `CapsuleConfig::include_memory_by_default`.
    pub include_memory: Option<bool>,
    /// Force or skip signing. `None` defers to `CapsuleConfig::auto_sign`.
    pub sign: Option<bool>,
    /// Output archive path. Defaults to `<agent_id>.capsule.tar.zst` in `cwd`.
    pub output_path: Option<PathBuf>,
    /// Required for `Replay` mode: the session whose latest checkpoint
    /// should be bundled. Ignored for other modes.
    pub session_id: Option<String>,
    /// Required for `Headless` mode: the root session whose scheduled
    /// jobs should be bundled. Ignored for other modes.
    pub root_session_id: Option<String>,
}

impl ExportRequest {
    pub fn new(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            revision_id: None,
            mode: CapsuleMode::Thin,
            include_memory: None,
            sign: None,
            output_path: None,
            session_id: None,
            root_session_id: None,
        }
    }
}

/// Result of a successful export.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExportOutcome {
    pub capsule_id: String,
    pub capsule_path: PathBuf,
    pub revision_id: String,
    pub mode: String,
    pub signed: bool,
    pub size_bytes: u64,
    pub manifest_digest: String,
    pub redactions: Vec<String>,
}

/// Inputs required to run the export pipeline. Carries handles, not raw
/// state, so callers can plug in their existing gateway containers.
pub struct ExportContext<'a> {
    pub gateway_dir: &'a Path,
    pub gateway_config: &'a GatewayConfig,
    pub gateway_store: &'a Arc<GatewayStore>,
}

/// Run the export pipeline. Returns the [`ExportOutcome`] and emits a
/// `capsule.export` causal event keyed to the agent.
pub fn export(req: ExportRequest, ctx: ExportContext<'_>) -> Result<ExportOutcome> {
    let cfg = &ctx.gateway_config.capsule;
    let revision = resolve_revision(&req, ctx.gateway_store.as_ref())?;
    let revision_dir = revision_dir(ctx.gateway_dir, &revision.agent_id, &revision.revision_id);

    // Hermetic/Replay capsules are imported offline, so every dependency must be
    // embedded as a pinned layer — a runtime-pip (dev-mode) closure can't be
    // reproduced without network on the receiver. Fail fast with guidance.
    require_locked_dependencies_for_hermetic(&revision_dir, req.mode)?;

    let staging = tempfile::tempdir().context("creating capsule staging dir")?;
    let staging_path = staging.path();

    let redactions = stage_revision_files(&revision_dir, staging_path)?;

    let included_skills = collect_skill_names(&revision_dir);

    let memory_snapshot = if req.include_memory.unwrap_or(cfg.include_memory_by_default) {
        Some(stage_memory_snapshot(
            staging_path,
            &revision.agent_id,
            ctx.gateway_store.as_ref(),
        )?)
    } else {
        None
    };

    let checkpoint_handle = if req.mode == CapsuleMode::Replay {
        let session_id = req.session_id.as_deref().ok_or_else(|| {
            anyhow::anyhow!("Replay-mode export requires session_id in ExportRequest")
        })?;
        match crate::runtime::checkpoint::load_latest_checkpoint(
            ctx.gateway_config,
            session_id,
        )? {
            Some(ckpt) => {
                // Refuse to bundle a checkpoint that belongs to a
                // different agent than the revision we're exporting —
                // otherwise the resulting capsule would carry agent A's
                // code with agent B's session state and produce
                // surprising restores on the receiver.
                if ckpt.agent_id != revision.agent_id {
                    anyhow::bail!(
                        "Replay-mode export: checkpoint for session {:?} belongs to agent {:?}, not {:?}",
                        session_id,
                        ckpt.agent_id,
                        revision.agent_id
                    );
                }
                let bytes = serde_json::to_vec_pretty(&ckpt)?;
                archive::write_entry(staging_path, crate::capsule::paths::CHECKPOINT_PATH, &bytes)?;
                Some(crate::capsule::paths::CHECKPOINT_PATH.to_string())
            }
            None => anyhow::bail!(
                "Replay-mode export: no checkpoint found for session {}",
                session_id
            ),
        }
    } else {
        None
    };

    let scheduled_jobs = if req.mode == CapsuleMode::Headless {
        let root = req.root_session_id.as_deref().ok_or_else(|| {
            anyhow::anyhow!("Headless-mode export requires root_session_id in ExportRequest")
        })?;
        ctx.gateway_store
            .list_scheduled_jobs_for_root(root)?
            .into_iter()
            .map(|j| autonoetic_types::capsule::CapsuleScheduledJob {
                job_id: j.job_id,
                owner_agent_id: j.owner_agent_id,
                root_session_id: j.root_session_id,
                target_agent_id: j.target_agent_id,
                target_revision_id: j.target_revision_id,
                message: j.message,
                metadata_json: j.metadata_json,
                cron_expr: j.cron_expr,
                timezone: j.timezone,
                created_at: j.created_at,
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let capsule_id = compute_capsule_id(&revision.revision_id, req.mode);
    let signed = req.sign.unwrap_or(cfg.auto_sign);

    let mut manifest = CapsuleManifest {
        capsule_id: capsule_id.clone(),
        format_version: CAPSULE_FORMAT_VERSION.to_string(),
        mode: req.mode,
        created_at: Utc::now().to_rfc3339(),
        agent_id: revision.agent_id.clone(),
        revision_id: revision.revision_id.clone(),
        revision_short_id: revision.short_id.clone(),
        content_digest: revision.content_digest.clone(),
        entrypoint: crate::capsule::paths::SKILL_REL.to_string(),
        runtime_lock: crate::capsule::paths::RUNTIME_LOCK_REL.to_string(),
        included_artifacts: vec![],
        included_layers: vec![],
        included_skills,
        gateway_runtime: None,
        memory_snapshot,
        checkpoint_handle,
        redactions: redactions.clone(),
        signature: None,
        provenance: CapsuleProvenance {
            origin_node_id: ctx.gateway_config.node_id.clone(),
            gateway_version: env!("CARGO_PKG_VERSION").to_string(),
            trust_domain: "local".to_string(),
            parent_capsule_id: None,
        },
        requires_agents: vec![],
        requires_skills: vec![],
        scheduled_jobs,
        platform: Some(autonoetic_types::capsule::CapsulePlatform {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
        }),
    };

    // Hermetic layer embedding via LayerStore is a follow-up — Phase 4
    // ships the export-side platform descriptor and the scheduled-jobs
    // bundle; the layer-closure traversal helper will land alongside the
    // OFP receive-side handler in a separate PR.

    if signed {
        let key = GatewayIdentityKey::load_or_generate(ctx.gateway_dir)
            .context("loading gateway identity key to sign capsule")?;
        manifest.signature = Some(verify::sign_manifest(&manifest, &key)?);
    }

    let manifest_digest = verify::manifest_digest(&manifest)?;
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    archive::write_entry(staging_path, "capsule.json", &manifest_bytes)?;

    let output_path = req.output_path.clone().unwrap_or_else(|| {
        PathBuf::from(format!("{}.capsule.tar.zst", revision.agent_id))
    });
    archive::pack(staging_path, &output_path)?;

    let size_bytes = std::fs::metadata(&output_path)?.len();
    if size_bytes > cfg.max_capsule_size_bytes {
        anyhow::bail!(
            "capsule archive size {} exceeds configured max {} (set capsule.max_capsule_size_bytes higher to allow)",
            size_bytes,
            cfg.max_capsule_size_bytes
        );
    }

    emit_export_event(
        ctx.gateway_store,
        &capsule_id,
        &revision.agent_id,
        &revision.revision_id,
        &manifest.mode,
        size_bytes,
        signed,
    )?;

    Ok(ExportOutcome {
        capsule_id,
        capsule_path: output_path,
        revision_id: revision.revision_id,
        mode: mode_str(manifest.mode).to_string(),
        signed,
        size_bytes,
        manifest_digest,
        redactions,
    })
}

fn resolve_revision(
    req: &ExportRequest,
    store: &GatewayStore,
) -> Result<autonoetic_types::agent_revision::AgentRevisionRecord> {
    if let Some(rev_id) = req.revision_id.as_deref() {
        return store
            .get_agent_revision(rev_id)?
            .with_context(|| format!("unknown revision: {}", rev_id));
    }
    let alias = store
        .get_agent_alias(&req.agent_id)?
        .with_context(|| format!("no alias for agent: {}", req.agent_id))?;
    store
        .get_agent_revision(&alias.revision_id)?
        .with_context(|| format!("alias points to missing revision: {}", alias.revision_id))
}

fn revision_dir(gateway_dir: &Path, agent_id: &str, revision_id: &str) -> PathBuf {
    gateway_dir
        .join("revisions")
        .join("agents")
        .join(agent_id)
        .join(revision_id)
}

fn stage_revision_files(revision_dir: &Path, staging: &Path) -> Result<Vec<String>> {
    let agent_root = staging.join("agent");
    std::fs::create_dir_all(&agent_root)?;
    let mut redactions = Vec::new();
    copy_dir_redacted(revision_dir, &agent_root, &mut redactions, "agent")?;
    Ok(redactions)
}

fn copy_dir_redacted(
    src: &Path,
    dst: &Path,
    redactions: &mut Vec<String>,
    rel_prefix: &str,
) -> Result<()> {
    for entry in std::fs::read_dir(src)
        .with_context(|| format!("reading revision dir {}", src.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        let dst_path = dst.join(&name);
        let rel = format!("{}/{}", rel_prefix, name_str);
        if file_type.is_dir() {
            std::fs::create_dir_all(&dst_path)?;
            copy_dir_redacted(&entry.path(), &dst_path, redactions, &rel)?;
        } else if file_type.is_file() {
            let bytes = std::fs::read(entry.path())?;
            let (out_bytes, redacted) = redact_file_if_text(&name_str, bytes);
            if redacted {
                redactions.push(rel);
            }
            std::fs::write(&dst_path, out_bytes)?;
        }
    }
    Ok(())
}

/// Cap on file size for the UTF-8 fallback scrubbing path. We don't want
/// to slurp huge binary-ish files through the regex pipeline.
const REDACTION_UTF8_FALLBACK_SIZE_LIMIT: usize = 1024 * 1024;

fn redact_file_if_text(filename: &str, bytes: Vec<u8>) -> (Vec<u8>, bool) {
    // First branch: a recognised text extension — always try to scrub.
    let has_text_extension = filename
        .rsplit_once('.')
        .map(|(_, ext)| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "md" | "txt" | "lock" | "json" | "yaml" | "yml" | "toml" | "py" | "sh" | "ts" | "js" | "rs"
            )
        })
        .unwrap_or(false);
    // Second branch: recognised extensionless configuration filenames
    // that often carry secrets (`.env`, `Dockerfile`, `Makefile`, etc.).
    let basename = filename
        .rsplit_once('/')
        .map(|(_, b)| b)
        .unwrap_or(filename);
    let is_known_textual_basename = matches!(
        basename,
        ".env"
            | "Dockerfile"
            | "Makefile"
            | "LICENSE"
            | "Containerfile"
            | "Procfile"
            | ".bashrc"
            | ".zshrc"
    ) || basename.starts_with(".env.");
    let try_redact = has_text_extension || is_known_textual_basename;
    if !try_redact {
        // Fall back to UTF-8 detection for anything else, but only up
        // to a size cap to keep the regex pipeline bounded.
        if bytes.len() > REDACTION_UTF8_FALLBACK_SIZE_LIMIT {
            return (bytes, false);
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return (bytes, false);
        };
        let redacted = redact_embedded_secrets(text);
        if redacted == text {
            return (bytes, false);
        }
        return (redacted.into_bytes(), true);
    }
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return (bytes, false);
    };
    let redacted = redact_embedded_secrets(text);
    let changed = redacted != text;
    (redacted.into_bytes(), changed)
}

fn collect_skill_names(revision_dir: &Path) -> Vec<String> {
    let skill_path = revision_dir.join("SKILL.md");
    if skill_path.is_file() {
        vec!["SKILL.md".to_string()]
    } else {
        Vec::new()
    }
}

fn stage_memory_snapshot(
    staging: &Path,
    agent_id: &str,
    store: &GatewayStore,
) -> Result<autonoetic_types::capsule::CapsuleMemorySnapshot> {
    // Enumerate memories owned by this agent (any scope), then run each
    // one through the redaction pipeline. The receiving gateway may
    // re-key these (see [`crate::capsule::import`]).
    let ids = store.memory_list_ids_owned_by(agent_id)?;
    let mut entries: Vec<serde_json::Value> = Vec::with_capacity(ids.len());
    let mut scopes: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for id in &ids {
        if let Some(obj) = store.memory_get_unrestricted(id)? {
            scopes.insert(obj.scope.clone());
            let serialised = serde_json::to_value(&obj)?;
            entries.push(redact_json_value(&serialised));
        }
    }
    let snapshot_json = serde_json::json!({
        "entries": entries,
        "scopes": scopes.iter().cloned().collect::<Vec<_>>(),
    });
    let serialised = serde_json::to_vec_pretty(&snapshot_json)?;
    archive::write_entry(staging, crate::capsule::paths::MEMORY_SNAPSHOT_PATH, &serialised)?;
    Ok(autonoetic_types::capsule::CapsuleMemorySnapshot {
        entry_count: entries.len() as u64,
        scopes: scopes.into_iter().collect(),
        content_handle: crate::capsule::paths::MEMORY_SNAPSHOT_PATH.to_string(),
        redacted: true,
    })
}

/// Content-derived capsule ID: SHA-256 over (revision_id, mode).
///
/// Deterministic so that two exports of the same revision in the same
/// mode produce the same `capsule_id` — necessary for dedup and stable
/// provenance chains (see `docs/cognitive-capsule.md`).
/// Timestamp salting is intentionally avoided.
fn compute_capsule_id(revision_id: &str, mode: CapsuleMode) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(revision_id.as_bytes());
    hasher.update(b"\x00");
    hasher.update(mode_str(mode).as_bytes());
    let hex = format!("{:x}", hasher.finalize());
    format!("cap_sha256:{}", &hex[..16])
}

fn mode_str(mode: CapsuleMode) -> &'static str {
    match mode {
        CapsuleMode::Thin => "thin",
        CapsuleMode::Hermetic => "hermetic",
        CapsuleMode::Replay => "replay",
        CapsuleMode::Headless => "headless",
    }
}

/// Hermetic/Replay capsules embed their closure for offline import, so the
/// agent must be **dependency-locked** (deps baked into pinned layers, no
/// runtime-pip step). Reject the export otherwise, with guidance. Thin/Headless
/// modes carry references and are unaffected. A missing/unparseable lock is not
/// blocked here (there's nothing we can positively flag as runtime-pip).
fn require_locked_dependencies_for_hermetic(
    revision_dir: &Path,
    mode: CapsuleMode,
) -> Result<()> {
    if !mode.is_hermetic() {
        return Ok(());
    }
    let lock_path = revision_dir.join("runtime.lock");
    let Ok(content) = std::fs::read_to_string(&lock_path) else {
        return Ok(());
    };
    let Ok(lock) = serde_yaml::from_str::<autonoetic_types::runtime_lock::RuntimeLock>(&content)
    else {
        return Ok(());
    };
    if lock.has_runtime_pip_dependencies() {
        anyhow::bail!(
            "{}-mode export requires a dependency-locked agent, but its runtime.lock declares \
             runtime-installed (pip) dependencies. Hermetic/Replay capsules import offline, so \
             dependencies must be baked into pinned layers first (locked mode). \
             See docs/rfc/portable-wasm-execution-tier.md §5.4.1.",
            mode_str(mode)
        );
    }
    Ok(())
}

fn emit_export_event(
    store: &GatewayStore,
    capsule_id: &str,
    agent_id: &str,
    revision_id: &str,
    mode: &CapsuleMode,
    size_bytes: u64,
    signed: bool,
) -> Result<()> {
    let payload = serde_json::json!({
        "capsule_id": capsule_id,
        "revision_id": revision_id,
        "mode": mode_str(*mode),
        "size_bytes": size_bytes,
        "signed": signed,
    });
    let event = autonoetic_types::causal_chain::CausalEventRecord {
        event_id: uuid::Uuid::new_v4().to_string(),
        agent_id: agent_id.to_string(),
        session_id: "gateway".to_string(),
        turn_id: None,
        event_seq: 0,
        timestamp: Utc::now().to_rfc3339(),
        category: "capsule".to_string(),
        action: "export".to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK_WITH_PIP: &str = "gateway:\n  artifact: \"\"\n  version: \"\"\n  sha256: \"\"\n  signature: null\nsdk:\n  version: \"\"\nsandbox:\n  backend: bubblewrap\ndependencies:\n  - runtime: python\n    packages: [requests]\n";
    const LOCK_NO_DEPS: &str = "gateway:\n  artifact: \"\"\n  version: \"\"\n  sha256: \"\"\n  signature: null\nsdk:\n  version: \"\"\nsandbox:\n  backend: bubblewrap\n";

    #[test]
    fn hermetic_export_rejects_runtime_pip_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("runtime.lock"), LOCK_WITH_PIP).unwrap();
        // Hermetic + Replay reject; Thin + Headless are unaffected.
        let err = require_locked_dependencies_for_hermetic(dir.path(), CapsuleMode::Hermetic)
            .unwrap_err()
            .to_string();
        assert!(err.contains("dependency-locked"), "got: {err}");
        assert!(require_locked_dependencies_for_hermetic(dir.path(), CapsuleMode::Replay).is_err());
        assert!(require_locked_dependencies_for_hermetic(dir.path(), CapsuleMode::Thin).is_ok());
        assert!(
            require_locked_dependencies_for_hermetic(dir.path(), CapsuleMode::Headless).is_ok()
        );
    }

    #[test]
    fn hermetic_export_allows_locked_closure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("runtime.lock"), LOCK_NO_DEPS).unwrap();
        assert!(require_locked_dependencies_for_hermetic(dir.path(), CapsuleMode::Hermetic).is_ok());
    }

    #[test]
    fn hermetic_export_lenient_when_no_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        // No runtime.lock present → nothing to positively flag as runtime-pip.
        assert!(require_locked_dependencies_for_hermetic(dir.path(), CapsuleMode::Hermetic).is_ok());
    }

    #[test]
    fn redact_file_if_text_masks_text_secrets() {
        let bytes = b"OPENAI_API_KEY=sk-abc123secret\n".to_vec();
        let (out, changed) = redact_file_if_text("SKILL.md", bytes.clone());
        assert!(changed);
        let out_str = std::str::from_utf8(&out).unwrap();
        assert!(
            !out_str.contains("sk-abc123secret"),
            "secret should be masked: {}",
            out_str
        );
    }

    #[test]
    fn redact_file_if_text_passes_binary_unchanged() {
        let bytes = b"\x7fELF\x02\x01".to_vec();
        let (out, changed) = redact_file_if_text("binary.so", bytes.clone());
        assert!(!changed);
        assert_eq!(out, bytes);
    }

    #[test]
    fn compute_capsule_id_format() {
        let id = compute_capsule_id("rev_sha256:abc", CapsuleMode::Thin);
        assert!(id.starts_with("cap_sha256:"));
        assert_eq!(id.len(), "cap_sha256:".len() + 16);
    }

    #[test]
    fn compute_capsule_id_is_deterministic_and_mode_sensitive() {
        let a = compute_capsule_id("rev_sha256:abc", CapsuleMode::Thin);
        let b = compute_capsule_id("rev_sha256:abc", CapsuleMode::Thin);
        let c = compute_capsule_id("rev_sha256:abc", CapsuleMode::Hermetic);
        let d = compute_capsule_id("rev_sha256:xyz", CapsuleMode::Thin);
        assert_eq!(a, b, "same inputs must yield same capsule_id");
        assert_ne!(a, c, "mode change must yield a different capsule_id");
        assert_ne!(a, d, "revision change must yield a different capsule_id");
    }

    #[test]
    fn mode_str_covers_all_variants() {
        assert_eq!(mode_str(CapsuleMode::Thin), "thin");
        assert_eq!(mode_str(CapsuleMode::Hermetic), "hermetic");
        assert_eq!(mode_str(CapsuleMode::Replay), "replay");
        assert_eq!(mode_str(CapsuleMode::Headless), "headless");
    }
}
