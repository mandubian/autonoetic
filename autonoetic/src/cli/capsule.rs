//! `autonoetic capsule` CLI subcommands — export/import/verify/inspect.
//!
//! These wrap the gateway-side pipeline in `autonoetic_gateway::capsule`.
//! Each subcommand opens the gateway store from `config.yaml`, runs the
//! requested operation, and prints either a human-readable summary or
//! JSON when `--json` is set.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use autonoetic_gateway::capsule::{self, ExportRequest, ImportRequest};
use autonoetic_gateway::config::load_config;
use autonoetic_gateway::execution::gateway_root_dir;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::capsule::{CapsuleManifest, CapsuleMode};

/// Parse a `--mode` value into [`CapsuleMode`].
pub fn parse_mode(s: &str) -> anyhow::Result<CapsuleMode> {
    match s.to_ascii_lowercase().as_str() {
        "thin" => Ok(CapsuleMode::Thin),
        "hermetic" => Ok(CapsuleMode::Hermetic),
        "replay" => Ok(CapsuleMode::Replay),
        "headless" => Ok(CapsuleMode::Headless),
        other => anyhow::bail!(
            "unknown capsule mode '{}' (expected one of: thin, hermetic, replay, headless)",
            other
        ),
    }
}

/// `autonoetic capsule export`.
#[allow(clippy::too_many_arguments)]
pub fn handle_export(
    config_path: &Path,
    agent_id: &str,
    mode: &str,
    revision: Option<&str>,
    include_memory: Option<bool>,
    sign: Option<bool>,
    output: Option<&Path>,
    json: bool,
) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let gateway_dir = gateway_root_dir(&config);
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let req = ExportRequest {
        agent_id: agent_id.to_string(),
        revision_id: revision.map(|s| s.to_string()),
        mode: parse_mode(mode)?,
        // `None` lets the export pipeline defer to
        // `config.capsule.include_memory_by_default` / `auto_sign`. The
        // CLI only forces a value when the operator explicitly passes
        // `--include-memory` / `--sign` (or `=false`).
        include_memory,
        sign,
        output_path: output.map(|p| p.to_path_buf()),
    };
    let outcome = capsule::export(
        req,
        capsule::ExportContext {
            gateway_dir: &gateway_dir,
            gateway_config: &config,
            gateway_store: &store,
        },
    )?;
    if json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    } else {
        println!("Capsule exported");
        println!("  capsule_id:    {}", outcome.capsule_id);
        println!("  revision_id:   {}", outcome.revision_id);
        println!("  mode:          {}", outcome.mode);
        println!("  signed:        {}", outcome.signed);
        println!("  size_bytes:    {}", outcome.size_bytes);
        println!("  output:        {}", outcome.capsule_path.display());
        println!("  digest:        sha256:{}", outcome.manifest_digest);
        if !outcome.redactions.is_empty() {
            println!("  redacted ({}):", outcome.redactions.len());
            for r in &outcome.redactions {
                println!("    - {}", r);
            }
        }
    }
    Ok(())
}

/// `autonoetic capsule import`.
#[allow(clippy::too_many_arguments)]
pub fn handle_import(
    config_path: &Path,
    archive: &Path,
    verify_signature: bool,
    activate: bool,
    dry_run: bool,
    trust_domain: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let gateway_dir = gateway_root_dir(&config);
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let req = ImportRequest {
        archive_path: archive.to_path_buf(),
        verify_signature,
        activate,
        dry_run,
        trust_domain_override: trust_domain.map(|s| s.to_string()),
    };
    let outcome = capsule::import(
        req,
        capsule::ImportContext {
            gateway_dir: &gateway_dir,
            gateway_config: &config,
            gateway_store: &store,
        },
    )?;
    if json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    } else {
        println!(
            "Capsule {}",
            if outcome.dry_run {
                "validated (dry-run)"
            } else if outcome.created_revision {
                "imported"
            } else {
                "imported (revision already present)"
            }
        );
        println!("  capsule_id:        {}", outcome.capsule_id);
        println!("  agent_id:          {}", outcome.agent_id);
        println!("  revision_id:       {}", outcome.revision_id);
        println!("  revision_short_id: {}", outcome.revision_short_id);
        println!("  signature_status:  {}", outcome.signature_status);
        println!(
            "  dedup_savings:     {} bytes",
            outcome.dedup_savings_bytes
        );
        println!("  created_revision:  {}", outcome.created_revision);
    }
    Ok(())
}

/// `autonoetic capsule verify`.
pub fn handle_verify(config_path: &Path, archive: &Path, json: bool) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let manifest = read_manifest(archive, config.capsule.max_capsule_size_bytes)?;
    let status = capsule::verify::verify_signature(&manifest, &config.capsule, false)?;
    let digest = capsule::verify::manifest_digest(&manifest)?;
    if json {
        let payload = serde_json::json!({
            "capsule_id": manifest.capsule_id,
            "format_version": manifest.format_version,
            "agent_id": manifest.agent_id,
            "revision_id": manifest.revision_id,
            "mode": manifest.mode,
            "manifest_digest": format!("sha256:{}", digest),
            "signature_status": format!("{:?}", status),
            "signer_id": manifest.signature.as_ref().map(|s| s.signer_id.clone()),
            "redactions": manifest.redactions,
            "provenance": manifest.provenance,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!("Capsule verification report");
        println!("  capsule_id:        {}", manifest.capsule_id);
        println!("  format_version:    {}", manifest.format_version);
        println!("  agent_id:          {}", manifest.agent_id);
        println!("  revision_id:       {}", manifest.revision_id);
        println!("  mode:              {:?}", manifest.mode);
        println!("  manifest_digest:   sha256:{}", digest);
        println!("  signature_status:  {:?}", status);
        if let Some(sig) = &manifest.signature {
            println!("  signer_id:         {}", sig.signer_id);
        }
        if !manifest.redactions.is_empty() {
            println!("  redactions ({}):", manifest.redactions.len());
            for r in &manifest.redactions {
                println!("    - {}", r);
            }
        }
    }
    // Exit non-zero on any failed-trust outcome. A signed but
    // tampered/unverifiable capsule must not fool scripts that drive
    // verification from CI.
    if matches!(
        status,
        capsule::verify::SignatureStatus::Mismatch
            | capsule::verify::SignatureStatus::UntrustedSigner
            | capsule::verify::SignatureStatus::Malformed
    ) {
        anyhow::bail!("capsule verification failed: {:?}", status);
    }
    Ok(())
}

/// `autonoetic capsule inspect`.
pub fn handle_inspect(config_path: &Path, archive: &Path, json: bool) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let manifest = read_manifest(archive, config.capsule.max_capsule_size_bytes)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&manifest)?);
    } else {
        println!("Capsule manifest summary");
        println!("  capsule_id:        {}", manifest.capsule_id);
        println!("  format_version:    {}", manifest.format_version);
        println!("  mode:              {:?}", manifest.mode);
        println!("  agent_id:          {}", manifest.agent_id);
        println!(
            "  revision_id:       {}  ({})",
            manifest.revision_id, manifest.revision_short_id
        );
        println!(
            "  content_digest:    {}",
            manifest.content_digest
        );
        println!("  created_at:        {}", manifest.created_at);
        println!("  entrypoint:        {}", manifest.entrypoint);
        println!("  runtime_lock:      {}", manifest.runtime_lock);
        println!(
            "  included artifacts:{}",
            if manifest.included_artifacts.is_empty() {
                " (none)".to_string()
            } else {
                format!(" {}", manifest.included_artifacts.len())
            }
        );
        println!(
            "  included layers:   {}",
            if manifest.included_layers.is_empty() {
                "(none)".to_string()
            } else {
                manifest.included_layers.len().to_string()
            }
        );
        println!(
            "  included skills:   {}",
            if manifest.included_skills.is_empty() {
                "(none)".to_string()
            } else {
                manifest.included_skills.join(", ")
            }
        );
        if let Some(mem) = &manifest.memory_snapshot {
            println!(
                "  memory snapshot:   {} entries, redacted={}",
                mem.entry_count, mem.redacted
            );
        }
        if let Some(ckpt) = &manifest.checkpoint_handle {
            println!("  checkpoint:        {}", ckpt);
        }
        println!(
            "  signature:         {}",
            manifest
                .signature
                .as_ref()
                .map(|s| format!("{} by {}", s.algorithm, s.signer_id))
                .unwrap_or_else(|| "none".to_string())
        );
        println!(
            "  origin_node:       {} (gateway {})",
            manifest.provenance.origin_node_id, manifest.provenance.gateway_version
        );
        println!("  trust_domain:      {}", manifest.provenance.trust_domain);
        if let Some(p) = &manifest.provenance.parent_capsule_id {
            println!("  parent_capsule:    {}", p);
        }
    }
    Ok(())
}

fn read_manifest(archive: &Path, max_extract_bytes: u64) -> anyhow::Result<CapsuleManifest> {
    let tmp = tempfile::tempdir()?;
    capsule::archive::unpack(archive, tmp.path(), max_extract_bytes)?;
    let bytes = capsule::archive::read_entry(tmp.path(), "capsule.json")?;
    let manifest: CapsuleManifest = serde_json::from_slice(&bytes)?;
    Ok(manifest)
}

#[allow(dead_code)]
pub(crate) fn _ensure_archive_path(p: Option<&Path>, agent_id: &str) -> PathBuf {
    p.map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from(format!("{}.capsule.tar.zst", agent_id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mode_accepts_all_canonical_strings() {
        assert!(matches!(parse_mode("thin").unwrap(), CapsuleMode::Thin));
        assert!(matches!(parse_mode("Hermetic").unwrap(), CapsuleMode::Hermetic));
        assert!(matches!(parse_mode("REPLAY").unwrap(), CapsuleMode::Replay));
        assert!(matches!(parse_mode("headless").unwrap(), CapsuleMode::Headless));
    }

    #[test]
    fn parse_mode_rejects_unknown() {
        let err = parse_mode("rocket").expect_err("must reject");
        assert!(err.to_string().contains("unknown capsule mode"));
    }
}
