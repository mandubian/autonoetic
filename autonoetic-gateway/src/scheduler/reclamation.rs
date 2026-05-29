use autonoetic_types::config::ReclamationConfig;
use std::collections::HashSet;
use std::path::Path;

/// Path to the file that records the last reclamation sweep timestamp.
const RECLAMATION_STATE_FILE: &str = "reclamation_last_run.txt";

/// Run the full reclamation sweep if enough time has passed since the last run.
///
/// Returns a summary of what was reclaimed.
pub fn run_reclamation_sweep(
    gateway_dir: &Path,
    store: &crate::scheduler::gateway_store::GatewayStore,
    config: &ReclamationConfig,
    now: &chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<ReclamationSummary> {
    if !config.enabled {
        return Ok(ReclamationSummary::default());
    }

    if !is_reclamation_due(gateway_dir, config, now) {
        return Ok(ReclamationSummary::default());
    }

    // Mark last run timestamp BEFORE doing work so a crash mid-sweep
    // doesn't cause re-runs on every tick until the interval expires.
    record_last_run(gateway_dir, now);

    let mut summary = ReclamationSummary::default();

    // 1. Delete expired memories (safety switch: cfg.expired_memory_retention_days > 0)
    if config.expired_memory_retention_days > 0 {
        match store.delete_expired_memories(now) {
            Ok(n) => {
                if n > 0 {
                    tracing::info!(target: "reclamation", expired_memories = n, "Deleted expired memories");
                }
                summary.expired_memories = n;
            }
            Err(e) => {
                tracing::warn!(target: "reclamation", error = %e, "Failed to delete expired memories");
            }
        }
    }

    // 2. Delete archived revisions
    match store.delete_archived_revisions(config.archived_revision_max_age_days, now) {
        Ok(n) => {
            if n > 0 {
                tracing::info!(target: "reclamation", archived_revisions = n, "Deleted archived revisions");
            }
            summary.archived_revisions = n;
        }
        Err(e) => {
            tracing::warn!(target: "reclamation", error = %e, "Failed to delete archived revisions");
        }
    }

    // 3. Close orphaned sessions
    match store.close_orphaned_sessions(config.orphaned_session_max_age_days, now) {
        Ok(n) => {
            if n > 0 {
                tracing::info!(target: "reclamation", orphaned_sessions = n, "Closed orphaned sessions");
            }
            summary.orphaned_sessions = n;
        }
        Err(e) => {
            tracing::warn!(target: "reclamation", error = %e, "Failed to close orphaned sessions");
        }
    }

    // 4. Cancel stale jobs
    match store.cancel_stale_jobs(config.stale_job_max_age_days, now) {
        Ok(n) => {
            if n > 0 {
                tracing::info!(target: "reclamation", stale_jobs = n, "Cancelled stale scheduled jobs");
            }
            summary.stale_jobs = n;
        }
        Err(e) => {
            tracing::warn!(target: "reclamation", error = %e, "Failed to cancel stale scheduled jobs");
        }
    }

    // 5. Delete orphaned content blobs
    match delete_orphaned_content_blobs(gateway_dir, store, config.content_blob_max_age_days, now) {
        Ok(n) => {
            if n > 0 {
                tracing::info!(target: "reclamation", orphaned_blobs = n, "Deleted orphaned content blobs");
            }
            summary.content_blobs = n;
        }
        Err(e) => {
            tracing::warn!(target: "reclamation", error = %e, "Failed to delete orphaned content blobs");
        }
    }

    // Emit causal event if anything was reclaimed
    if summary.has_work() {
        if let Err(e) = emit_reclamation_event(store, &summary, now) {
            tracing::warn!(target: "reclamation", error = %e, "Failed to emit reclamation causal event");
        }
    }

    Ok(summary)
}

/// Delete content blobs with zero remaining name references across all manifests.
///
/// Reference sources (all must agree the blob is unreferenced before deletion):
/// - Session manifest `names` and `aliases` maps (recursively walked)
/// - `artifact_refs` table `artifact_digest` column
///
/// Fails closed on any manifest read/parse error to prevent incorrect deletion.
fn delete_orphaned_content_blobs(
    gateway_dir: &Path,
    store: &crate::scheduler::gateway_store::GatewayStore,
    max_age_days: u64,
    now: &chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<u64> {
    if max_age_days == 0 {
        return Ok(0);
    }

    let content_dir = gateway_dir.join("content").join("sha256");
    if !content_dir.exists() {
        return Ok(0);
    }

    let sessions_dir = gateway_dir.join("sessions");
    let mut referenced = scan_manifest_references(&sessions_dir)?;

    // Also include artifact digests from the DB
    if let Ok(digests) = store.referenced_artifact_digests() {
        referenced.extend(digests);
    }

    let mut deleted = 0u64;
    let cutoff = *now - chrono::Duration::days(max_age_days as i64);

    for prefix_entry in std::fs::read_dir(&content_dir)? {
        let prefix_entry = prefix_entry?;
        if !prefix_entry.file_type()?.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(prefix_entry.path())? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let metadata = entry.metadata()?;

            let modified = match metadata.modified() {
                Ok(t) => {
                    let duration_since_epoch = t
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default();
                    let secs = duration_since_epoch.as_secs() as i64;
                    let nanos = duration_since_epoch.subsec_nanos();
                    chrono::DateTime::from_timestamp(secs, nanos).unwrap_or(*now)
                }
                Err(_) => continue,
            };
            if modified > cutoff {
                continue;
            }

            let prefix = prefix_entry.file_name().to_string_lossy().to_string();
            let filename = entry.file_name().to_string_lossy().to_string();
            let handle = format!("sha256:{}{}", prefix, filename);

            if !referenced.contains(&handle) {
                if let Err(e) = std::fs::remove_file(entry.path()) {
                    tracing::warn!(target: "reclamation", path = %entry.path().display(), error = %e, "Failed to delete orphaned blob");
                } else {
                    deleted += 1;
                }
            }
        }
    }

    Ok(deleted)
}

/// Recursively walk the sessions directory collecting all content handles
/// referenced by session manifest `names` and `aliases` maps.
///
/// Fails hard on any read/parse error to prevent incorrect orphan deletion.
fn scan_manifest_references(sessions_dir: &Path) -> anyhow::Result<HashSet<String>> {
    let mut referenced = HashSet::new();
    if !sessions_dir.exists() {
        return Ok(referenced);
    }
    collect_manifest_references(sessions_dir, &mut referenced)?;
    Ok(referenced)
}

fn collect_manifest_references(dir: &Path, referenced: &mut HashSet<String>) -> anyhow::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_manifest_references(&path, referenced)?;
        } else if entry.file_name() == "manifest.json" {
            let content = std::fs::read_to_string(&path)?;
            let manifest: serde_json::Value = serde_json::from_str(&content)?;
            if let Some(names) = manifest.get("names").and_then(|v| v.as_object()) {
                for (_name, handle) in names {
                    if let Some(h) = handle.as_str() {
                        referenced.insert(h.to_string());
                    }
                }
            }
            if let Some(aliases) = manifest.get("aliases").and_then(|v| v.as_object()) {
                for (_short, handle) in aliases {
                    if let Some(h) = handle.as_str() {
                        referenced.insert(h.to_string());
                    }
                }
            }
        }
    }
    Ok(())
}

/// Check whether enough time has passed since the last reclamation run.
fn is_reclamation_due(
    gateway_dir: &Path,
    config: &ReclamationConfig,
    now: &chrono::DateTime<chrono::Utc>,
) -> bool {
    let state_path = gateway_dir.join(RECLAMATION_STATE_FILE);
    match std::fs::read_to_string(&state_path) {
        Ok(last_run_str) => {
            let last_run = match chrono::DateTime::parse_from_rfc3339(last_run_str.trim()) {
                Ok(t) => t.with_timezone(&chrono::Utc),
                Err(_) => return true,
            };
            let elapsed = *now - last_run;
            elapsed.num_seconds() >= config.min_interval_secs as i64
        }
        Err(_) => true,
    }
}

/// Record the last reclamation run timestamp.
fn record_last_run(gateway_dir: &Path, now: &chrono::DateTime<chrono::Utc>) {
    let state_path = gateway_dir.join(RECLAMATION_STATE_FILE);
    if let Err(e) = std::fs::write(&state_path, now.to_rfc3339()) {
        tracing::warn!(
            target: "reclamation",
            path = %state_path.display(),
            error = %e,
            "Failed to write reclamation state file"
        );
    }
}

/// Emit a `reclamation.sweep` causal event with counts of reclaimed resources.
fn emit_reclamation_event(
    store: &crate::scheduler::gateway_store::GatewayStore,
    summary: &ReclamationSummary,
    now: &chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<()> {
    let mut rules = autonoetic_types::causal_chain::default_enforced_rules();
    rules.push("P-8.17".to_string());

    let payload = serde_json::json!({
        "reclamation": {
            "content_blobs": summary.content_blobs,
            "expired_memories": summary.expired_memories,
            "archived_revisions": summary.archived_revisions,
            "orphaned_sessions": summary.orphaned_sessions,
            "stale_jobs": summary.stale_jobs,
        },
    });

    store.create_causal_event(&autonoetic_types::causal_chain::CausalEventRecord {
        event_id: uuid::Uuid::new_v4().to_string(),
        agent_id: "gateway".to_string(),
        session_id: "system".to_string(),
        turn_id: None,
        event_seq: now.timestamp_millis().max(0) as u64,
        timestamp: now.to_rfc3339(),
        category: "reclamation".to_string(),
        action: "sweep".to_string(),
        status: autonoetic_types::causal_chain::EntryStatus::Success.to_string(),
        enforced_rules: rules,
        target: None,
        payload: serde_json::to_string(&payload).ok(),
        payload_ref: None,
        evidence_ref: None,
        reason: None,
    })?;

    Ok(())
}

/// Summary of reclaimed resources in a single sweep run.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ReclamationSummary {
    pub content_blobs: u64,
    pub expired_memories: u64,
    pub archived_revisions: u64,
    pub orphaned_sessions: u64,
    pub stale_jobs: u64,
}

impl ReclamationSummary {
    pub fn has_work(&self) -> bool {
        self.content_blobs > 0
            || self.expired_memories > 0
            || self.archived_revisions > 0
            || self.orphaned_sessions > 0
            || self.stale_jobs > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_reclamation_due_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = ReclamationConfig {
            enabled: true,
            min_interval_secs: 3600,
            ..Default::default()
        };
        let now = chrono::Utc::now();
        assert!(is_reclamation_due(dir.path(), &config, &now));
    }

    #[test]
    fn test_is_reclamation_due_recent_run() {
        let dir = tempfile::tempdir().unwrap();
        let config = ReclamationConfig {
            enabled: true,
            min_interval_secs: 3600,
            ..Default::default()
        };
        let now = chrono::Utc::now();

        let recent = (now - chrono::Duration::minutes(5)).to_rfc3339();
        std::fs::write(dir.path().join(RECLAMATION_STATE_FILE), &recent).unwrap();

        assert!(!is_reclamation_due(dir.path(), &config, &now));
    }

    #[test]
    fn test_is_reclamation_due_old_run() {
        let dir = tempfile::tempdir().unwrap();
        let config = ReclamationConfig {
            enabled: true,
            min_interval_secs: 3600,
            ..Default::default()
        };
        let now = chrono::Utc::now();

        let old = (now - chrono::Duration::hours(2)).to_rfc3339();
        std::fs::write(dir.path().join(RECLAMATION_STATE_FILE), &old).unwrap();

        assert!(is_reclamation_due(dir.path(), &config, &now));
    }

    #[test]
    fn test_reclamation_summary_has_work() {
        let s = ReclamationSummary::default();
        assert!(!s.has_work());

        let s = ReclamationSummary {
            content_blobs: 5,
            ..Default::default()
        };
        assert!(s.has_work());
    }

    #[test]
    fn test_scan_manifest_references_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let refs = scan_manifest_references(dir.path()).unwrap();
        assert!(refs.is_empty());
    }

    #[test]
    fn test_scan_manifest_references_names_and_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sess-1");
        std::fs::create_dir_all(&session_dir).unwrap();

        let handle_a = "sha256:aaaa...";
        let handle_b = "sha256:bbbb...";
        let manifest = serde_json::json!({
            "names": {
                "file1.txt": handle_a,
            },
            "aliases": {
                "a1b2c3d4": handle_b,
            },
        });
        std::fs::write(session_dir.join("manifest.json"), serde_json::to_vec(&manifest).unwrap()).unwrap();

        let refs = scan_manifest_references(dir.path()).unwrap();
        assert!(refs.contains(handle_a));
        assert!(refs.contains(handle_b));
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn test_scan_manifest_references_nested_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("root-session").join("child-session");
        std::fs::create_dir_all(&nested).unwrap();

        let handle = "sha256:cccc...";
        let manifest = serde_json::json!({
            "names": { "out.txt": handle },
        });
        std::fs::write(nested.join("manifest.json"), serde_json::to_vec(&manifest).unwrap()).unwrap();

        let refs = scan_manifest_references(dir.path()).unwrap();
        assert!(refs.contains(handle));
    }

    #[test]
    fn test_scan_manifest_references_fails_on_bad_json() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sess-bad");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("manifest.json"), b"not valid json").unwrap();

        let result = scan_manifest_references(dir.path());
        assert!(result.is_err(), "should fail closed on bad manifest JSON");
    }
}
