//! CLI handlers for `autonoetic eval` commands.
//!
//! Since #1119 (tranche 3) the DB lookups (fixture set + owning recording
//! session) go over JSON-RPC (`recording.fixture_set` / `recording.get`).
//! Staging fixture files stays local: it is file I/O under the gateway dir,
//! not database access, and the files can be large.

use anyhow::Context;
use std::path::Path;

use crate::cli::rpc::GatewayRpc;

pub fn handle_eval_sealed(
    config_path: &Path,
    artifact_ref: &str,
    fixture_set_id: &str,
    agent_id: &str,
    json: bool,
    _timeout: u64,
) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let rpc = GatewayRpc::from_config(&config)?;

    // 1. Look up the FixtureSet.
    let fixture_set: autonoetic_types::recording::FixtureSet = serde_json::from_value(
        rpc.call(
            "recording.fixture_set",
            serde_json::json!({ "fixture_set_id": fixture_set_id }),
        )
        .with_context(|| {
            format!("Fixture set '{}' not found or gateway unreachable", fixture_set_id)
        })?,
    )?;

    // 2. Look up the RecordingSession to get the staging directory path.
    let session_raw = rpc
        .call(
            "recording.get",
            serde_json::json!({ "session_id": fixture_set.recording_session_id }),
        )
        .with_context(|| {
            format!(
                "Recording session '{}' (for fixture set '{}') not found or gateway unreachable",
                fixture_set.recording_session_id, fixture_set_id
            )
        })?;
    let recording_session: autonoetic_types::recording::RecordingSession =
        serde_json::from_value(session_raw["session"].clone())?;

    // 3. Compute the fixture staging directory.
    let fixture_staging_dir = gateway_dir
        .join("recordings")
        .join(&recording_session.session_id)
        .join("fixtures");

    anyhow::ensure!(
        fixture_staging_dir.exists(),
        "Fixture staging directory not found at: {}",
        fixture_staging_dir.display()
    );

    // 4. Pre-populate the artifact's fixture temp directory.
    // The artifact ID is derived from the artifact ref (strip prefix).
    let artifact_id = artifact_ref.trim_start_matches("ar_");
    let temp_base = std::env::temp_dir()
        .join("autonoetic_artifact")
        .join(artifact_id.replace('/', "_"));
    let fixtures_dest = temp_base.join("fixtures");
    if fixtures_dest.exists() {
        std::fs::remove_dir_all(&fixtures_dest)?;
    }
    let copied = copy_fixture_dir(&fixture_staging_dir, &fixtures_dest)?;

    eprintln!(
        "  Pre-populated {} fixtures from '{}' into artifact fixture root.",
        copied, fixture_set_id
    );
    eprintln!(
        "  Artifact: {}  |  Agent: {}  |  Fixtures: {} across {} hosts",
        artifact_ref, agent_id, fixture_set.fixture_file_count, fixture_set.host_count
    );

    if json {
        let output = serde_json::json!({
            "ok": true,
            "fixture_set_id": fixture_set_id,
            "artifact_ref": artifact_ref,
            "agent_id": agent_id,
            "fixture_count": copied,
            "artifact_id": artifact_id,
            "message": format!(
                "Fixtures pre-populated. Run 'autonoetic agent run {}' to evaluate.",
                agent_id
            ),
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if !fixture_set.host_summary.is_empty() {
        eprintln!("  Hosts: {:?}", fixture_set.host_summary);
    }

    Ok(())
}

/// Recursively copy fixture files from source to destination. Returns count.
fn copy_fixture_dir(src: &Path, dst: &Path) -> anyhow::Result<u64> {
    if !src.exists() {
        return Ok(0);
    }
    std::fs::create_dir_all(dst)?;
    let mut count = 0;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let dest_path = dst.join(&file_name);
        if path.is_dir() {
            count += copy_fixture_dir(&path, &dest_path)?;
        } else {
            std::fs::copy(&path, &dest_path)?;
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn copy_fixture_dir_copies_files_recursively() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();

        // Create source fixture files.
        std::fs::create_dir_all(src.path().join("api.example.com")).unwrap();
        std::fs::write(
            src.path().join("api.example.com").join("GET-items.json"),
            r#"{"status":200,"body":"ok"}"#,
        )
        .unwrap();
        std::fs::write(
            src.path().join("api.example.com").join("POST-submit.json"),
            r#"{"status":201,"body":"created"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(src.path().join("auth.example.com-443")).unwrap();
        std::fs::write(
            src
                .path()
                .join("auth.example.com-443")
                .join("POST-login.json"),
            r#"{"status":200,"body":"token"}"#,
        )
        .unwrap();

        let count = copy_fixture_dir(src.path(), dst.path()).unwrap();
        assert_eq!(count, 3, "should copy 3 fixture files");

        assert!(dst.path().join("api.example.com").join("GET-items.json").exists());
        assert!(dst.path().join("api.example.com").join("POST-submit.json").exists());
        assert!(dst.path().join("auth.example.com-443").join("POST-login.json").exists());
    }

    #[test]
    fn copy_fixture_dir_empty_source_returns_zero() {
        let src = tempdir().unwrap();
        let dst = tempdir().unwrap();
        let count = copy_fixture_dir(src.path(), dst.path()).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn copy_fixture_dir_nonexistent_source_returns_zero() {
        let dst = tempdir().unwrap();
        let count = copy_fixture_dir(&Path::new("/nonexistent/path"), dst.path()).unwrap();
        assert_eq!(count, 0);
    }
}
