//! End-to-end CLI test for the `autonoetic capsule` subcommands.
//!
//! Seeds a revision via the gateway library, then drives the CLI binary
//! to export → inspect → verify → import (`--dry-run`).

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent_revision::{
    AgentAliasRecord, AgentRevisionRecord, AgentRevisionStatus,
};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

fn run_cli(config_path: &str, extra: &[&str]) -> Output {
    let bin = env!("CARGO_BIN_EXE_autonoetic");
    let mut cmd = Command::new(bin);
    cmd.args(["--config", config_path]);
    cmd.args(extra);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.spawn()
        .expect("spawn autonoetic")
        .wait_with_output()
        .expect("wait")
}

fn tmp_dir() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "autonoetic-capsule-e2e-{}",
        uuid::Uuid::new_v4().to_string()[..8].to_string()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn seed_revision(gateway_dir: &std::path::Path, agent_id: &str, revision_id: &str) {
    let rev_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join(agent_id)
        .join(revision_id);
    std::fs::create_dir_all(&rev_dir).unwrap();
    std::fs::write(rev_dir.join("SKILL.md"), "# test agent\n").unwrap();
    std::fs::write(rev_dir.join("runtime.lock"), "agents: []\n").unwrap();

    let store = Arc::new(GatewayStore::open(gateway_dir).unwrap());
    let rev = AgentRevisionRecord {
        revision_id: revision_id.to_string(),
        agent_id: agent_id.to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest: format!("sha256:{}", "a".repeat(64)),
        runtime_lock_hash: format!("sha256:{}", "b".repeat(64)),
        manifest_hash: format!("sha256:{}", "c".repeat(64)),
        created_at: "2026-05-28T00:00:00Z".to_string(),
        created_by_type: "user".to_string(),
        created_by_id: "test".to_string(),
        source_kind: "artifact".to_string(),
        source_ref: None,
        origin_node_id: "node-A".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Ready,
        metadata_json: serde_json::Value::Null,
        short_id: "abcd1234".to_string(),
        signature: None,
        signer_id: None,
    };
    store.insert_agent_revision(&rev).unwrap();
    let alias = AgentAliasRecord {
        alias_id: agent_id.to_string(),
        agent_id: agent_id.to_string(),
        revision_id: revision_id.to_string(),
        updated_at: "2026-05-28T00:00:00Z".to_string(),
        updated_by_type: "user".to_string(),
        updated_by_id: "test".to_string(),
        reason: None,
    };
    store.upsert_agent_alias(&alias).unwrap();
}

#[test]
fn cli_export_inspect_verify_dryrun_import_roundtrip() {
    let tmp = tmp_dir();
    let agents_dir = tmp.join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let config_path = tmp.join("config.yaml");
    let yaml = format!(
        "agents_dir: \"{}\"\nllm_presets:\n  default:\n    provider: openai\n    model: gpt-4o\nllm_preset_mapping:\n  default: default\n",
        agents_dir.display()
    );
    std::fs::write(&config_path, yaml).unwrap();

    seed_revision(&gateway_dir, "demo.agent", "rev_sha256:cap-cli-001");

    let archive = tmp.join("demo.capsule.tar.zst");

    // Export.
    let out = run_cli(
        config_path.to_str().unwrap(),
        &[
            "capsule",
            "export",
            "demo.agent",
            "--mode",
            "thin",
            "--sign",
            "--output",
            archive.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "export failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Capsule exported"), "{}", stdout);
    assert!(archive.exists(), "archive missing at {}", archive.display());

    // Inspect.
    let out = run_cli(
        config_path.to_str().unwrap(),
        &["capsule", "inspect", archive.to_str().unwrap()],
    );
    assert!(out.status.success(), "inspect failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("demo.agent"), "{}", stdout);

    // Verify (no signature requirement; should still succeed with Absent/Verified).
    let out = run_cli(
        config_path.to_str().unwrap(),
        &["capsule", "verify", archive.to_str().unwrap()],
    );
    assert!(out.status.success(), "verify failed");

    // Import --dry-run on a fresh gateway dir.
    let other = tmp_dir();
    let other_agents = other.join("agents");
    let other_gateway = other_agents.join(".gateway");
    std::fs::create_dir_all(&other_gateway).unwrap();
    let other_cfg = other.join("config.yaml");
    let yaml = format!("agents_dir: \"{}\"\n", other_agents.display());
    std::fs::write(&other_cfg, yaml).unwrap();
    let out = run_cli(
        other_cfg.to_str().unwrap(),
        &[
            "capsule",
            "import",
            archive.to_str().unwrap(),
            "--dry-run",
        ],
    );
    assert!(
        out.status.success(),
        "import dry-run failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("dry-run"), "{}", stdout);

    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(&other);
}
