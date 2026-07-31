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
use autonoetic_types::egress::Sink;
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
    /// Egress sink the capsule is destined for (RFC §7). When unset, inferred
    /// from [`Self::trust_domain`] via [`infer_capsule_destination_sink`].
    pub destination_sink: Option<Sink>,
    /// Trust domain for provenance and destination-sink inference (`local`,
    /// `partner`, `foreign`, …). Defaults to `"local"`.
    pub trust_domain: Option<String>,
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
            destination_sink: None,
            trust_domain: None,
        }
    }
}

/// Infer the egress destination sink for a capsule from its trust domain.
pub fn infer_capsule_destination_sink(trust_domain: &str) -> Sink {
    match trust_domain.trim().to_ascii_lowercase().as_str() {
        "local" => Sink::LocalAgent,
        "partner" => Sink::FederatedAgent,
        _ => Sink::RemoteModel,
    }
}

/// Resolve the effective destination sink for memory filtering.
pub fn resolve_capsule_destination_sink(
    explicit: Option<Sink>,
    trust_domain: Option<&str>,
) -> Sink {
    explicit.unwrap_or_else(|| {
        infer_capsule_destination_sink(trust_domain.unwrap_or("local"))
    })
}

fn sink_wire_name(sink: Sink) -> String {
    serde_json::to_value(sink)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "remote_model".to_string())
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
    /// Memory entries withheld because their egress label excluded the destination sink.
    pub memory_withheld_count: u64,
    pub destination_sink: String,
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

    let trust_domain = req
        .trust_domain
        .as_deref()
        .unwrap_or("local")
        .to_string();
    let destination_sink = resolve_capsule_destination_sink(
        req.destination_sink,
        Some(trust_domain.as_str()),
    );
    let destination_sink_name = sink_wire_name(destination_sink);

    let (memory_snapshot, memory_withheld_count) =
        if req.include_memory.unwrap_or(cfg.include_memory_by_default) {
            stage_memory_snapshot(
                staging_path,
                &revision.agent_id,
                ctx.gateway_store.as_ref(),
                destination_sink,
                &ctx.gateway_config.egress,
            )?
        } else {
            (None, 0)
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
                // A checkpoint carries `history: Vec<Message>` — every tool
                // result verbatim, including content the LLM chokepoint
                // withholds from remote providers. It is the most sensitive
                // payload a capsule can hold and, unlike `memory_snapshot`
                // above, nothing filtered it. Gate it on the session's taint
                // against the capsule's destination sink (RFC §7 / P-15.2).
                guard_replay_checkpoint_egress(&ctx, session_id, &ckpt, destination_sink)?;
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
            trust_domain: trust_domain.clone(),
            destination_sink: Some(destination_sink_name.clone()),
            memory_withheld_count,
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
        &destination_sink_name,
        memory_withheld_count,
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
        memory_withheld_count,
        destination_sink: destination_sink_name,
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

/// Refuse a Replay export whose checkpoint history is not cleared for the
/// capsule's destination sink (RFC §7 / P-15.2, #987).
///
/// The label consulted is the **union** of two sources, because either alone
/// can miss:
///
/// - the session's accumulated taint (`session_egress_taint`), which is what
///   every other off-machine boundary gates on; and
/// - the checkpoint's own `egress_labels` sidecar, which records the label of
///   each tool result actually present in `history`. A checkpoint can outlive
///   the taint row — a forked or restored session carries the sidecar with it —
///   so the sidecar is authoritative about what these bytes contain even when
///   the session row says nothing.
///
/// **Refuse rather than filter.** A Replay capsule exists to replay; a history
/// with holes punched in it is not replayable, so a silently-partial capsule
/// would be a worse artifact than an absent one. The operator's ways forward are
/// a non-Replay mode (thin/hermetic carry no history), a `local` trust domain,
/// or declassifying the session.
fn guard_replay_checkpoint_egress(
    ctx: &ExportContext<'_>,
    session_id: &str,
    ckpt: &crate::runtime::checkpoint::SessionCheckpoint,
    destination_sink: Sink,
) -> Result<()> {
    use crate::runtime::egress_labeler as el;

    // Fail closed: an unreadable taint must not export as if it were clean.
    let mut effective = el::require_boundary_session_taint(
        None,
        Some(ctx.gateway_store.as_ref()),
        Some(session_id),
    )
    .with_context(|| {
        format!(
            "Replay-mode export: cannot confirm the egress taint of session {session_id}; \
             refusing to bundle its history"
        )
    })?;
    for label in ckpt.egress_labels.values() {
        effective = effective.restrict(label);
    }

    if effective.allows(destination_sink) {
        return Ok(());
    }

    el::emit_surface_boundary_refused(
        ctx.gateway_store,
        session_id,
        &ckpt.agent_id,
        None,
        "capsule",
        &effective,
        &[],
        "capsule_replay_checkpoint_egress_refused",
    );

    anyhow::bail!(
        "capsule_replay_checkpoint_egress_refused: this session's history is labeled {} and \
         the capsule's destination sink is {}. A Replay capsule embeds the full conversation \
         history, so exporting it would move that content off this machine. Export in thin or \
         hermetic mode (neither carries history), target a `local` trust domain, or declassify \
         the session first.",
        autonoetic_types::egress::label_display_name(&effective),
        sink_wire_name(destination_sink),
    )
}

fn stage_memory_snapshot(
    staging: &Path,
    agent_id: &str,
    store: &GatewayStore,
    destination_sink: Sink,
    egress_cfg: &autonoetic_types::egress::EgressConfig,
) -> Result<(Option<autonoetic_types::capsule::CapsuleMemorySnapshot>, u64)> {
    // Enumerate memories owned by this agent (any scope), include only those
    // whose egress label permits the capsule's declared destination sink.
    let ids = store.memory_list_ids_owned_by(agent_id)?;
    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut scopes: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut withheld_count = 0u64;
    for id in &ids {
        if let Some(obj) = store.memory_get_unrestricted(id)? {
            let label = crate::runtime::egress_stored::resolve_stored_label(
                obj.egress_label.as_ref(),
                egress_cfg,
            );
            if !crate::runtime::egress_stored::stored_allows_sink(&label, destination_sink) {
                withheld_count += 1;
                continue;
            }
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
    Ok((
        Some(autonoetic_types::capsule::CapsuleMemorySnapshot {
            entry_count: entries.len() as u64,
            scopes: scopes.into_iter().collect(),
            content_handle: crate::capsule::paths::MEMORY_SNAPSHOT_PATH.to_string(),
            redacted: true,
            withheld_count,
        }),
        withheld_count,
    ))
}

/// Content-derived capsule ID: SHA-256 over (revision_id, mode).
///
/// Deterministic so that two exports of the same revision in the same
/// mode produce the same `capsule_id` — necessary for dedup and stable
/// provenance chains (see `docs/cognitive-capsule.md`).
/// Timestamp salting is intentionally avoided.
fn compute_capsule_id(revision_id: &str, mode: CapsuleMode) -> String {
    // `cap_sha256:<sha256(revision_id \0 mode)[..16]>` — deterministic, no salt.
    // Hashing the NUL-joined concatenation matches the previous streamed updates.
    format!(
        "cap_sha256:{}",
        autonoetic_types::id_format::hash_and_truncate(
            &format!("{revision_id}\0{}", mode_str(mode)),
            16
        )
    )
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
    destination_sink: &str,
    memory_withheld_count: u64,
) -> Result<()> {
    let payload = serde_json::json!({
        "capsule_id": capsule_id,
        "revision_id": revision_id,
        "mode": mode_str(*mode),
        "size_bytes": size_bytes,
        "signed": signed,
        "destination_sink": destination_sink,
        "memory_withheld_count": memory_withheld_count,
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
    fn infer_capsule_destination_sink_maps_trust_domains() {
        use autonoetic_types::egress::Sink;
        assert_eq!(
            infer_capsule_destination_sink("local"),
            Sink::LocalAgent
        );
        assert_eq!(
            infer_capsule_destination_sink("partner"),
            Sink::FederatedAgent
        );
        assert_eq!(
            infer_capsule_destination_sink("foreign"),
            Sink::RemoteModel
        );
    }

    #[test]
    fn resolve_capsule_destination_sink_prefers_explicit() {
        use autonoetic_types::egress::Sink;
        assert_eq!(
            resolve_capsule_destination_sink(Some(Sink::Network), Some("local")),
            Sink::Network
        );
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
