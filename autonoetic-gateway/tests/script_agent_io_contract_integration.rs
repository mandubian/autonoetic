//! io contract enforcement for candidate-revision (smoke-test) spawns.
//!
//! Regression for the session-b5c8f091 incident: a candidate script agent
//! passed its pre-promotion smoke test even though its stdout violated the
//! declared `io.returns` schema (missing required `status` field). Response
//! validation resolved the manifest via the install alias — which does not
//! exist for unpromoted candidates — so validation was silently skipped and
//! the schema violation only surfaced on first production use.
//!
//! These tests pin the fixed behavior:
//!  1. A candidate-revision spawn (no alias installed) validates script stdout
//!     against the revision's own `io.returns` — violations fail the spawn.
//!  2. Conforming output passes.
//!  3. `agent.spawn` input validation (`io.accepts`) reads the candidate
//!     revision's SKILL.md, not the (absent/stale) live agent dir.

mod support;

use std::sync::Arc;

use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::agent::AgentSpawnTool;
use autonoetic_gateway::runtime::tools::NativeTool;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent_revision::{AgentRevisionRecord, AgentRevisionStatus};
use autonoetic_types::principal::PrincipalKind;
use support::TestWorkspace;

const VIOLATING_AGENT: &str = "candidate-violating";
const CONFORMING_AGENT: &str = "candidate-conforming";
const REVISION_ID: &str = "rev_candidate_iotest";

fn write_script_agent_dir(
    agent_dir: &std::path::Path,
    agent_id: &str,
    script_stdout: &str,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(agent_dir.join("scripts"))?;
    std::fs::write(
        agent_dir.join("scripts/emit.py"),
        format!(
            r#"#!/usr/bin/env python3
import os
_ = os.environ.get("AUTONOETIC_INPUT", "")
print('{script_stdout}')
"#,
        ),
    )?;
    let skill_md = format!(
        r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "{agent_id}"
  name: "{agent_id}"
  description: "Candidate script agent for io contract tests"
execution_mode: script
script_entry: scripts/emit.py
capabilities: []
io:
  accepts:
    type: object
    required: [city]
    properties:
      city:
        type: string
  returns:
    type: object
    required: [status]
    properties:
      status:
        type: string
      forecast:
        type: array
---
# Candidate Script Agent
"#,
    );
    std::fs::write(agent_dir.join("SKILL.md"), skill_md)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;
    Ok(())
}

/// Seed a revision directory + record in `Candidate` status with NO install
/// alias — the exact state of a pre-promotion smoke-test target.
fn seed_candidate_revision(
    store: &GatewayStore,
    config: &autonoetic_types::config::GatewayConfig,
    agent_id: &str,
    agent_dir: &std::path::Path,
) -> anyhow::Result<String> {
    let gateway_dir = config.agents_dir.join(".gateway");
    let rev_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join(agent_id)
        .join(REVISION_ID);
    std::fs::create_dir_all(&rev_dir)?;
    for entry in std::fs::read_dir(agent_dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        if path.is_dir() {
            copy_dir_all(&path, &rev_dir.join(file_name))?;
        } else {
            std::fs::copy(&path, rev_dir.join(file_name))?;
        }
    }
    // Scripts need +x inside the sandbox.
    let script_path = rev_dir.join("scripts/emit.py");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path)?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(&script_path, perms)?;
    }

    let rec = AgentRevisionRecord {
        revision_id: REVISION_ID.to_string(),
        agent_id: agent_id.to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest: format!("sha256:candidate-{agent_id}"),
        runtime_lock_hash: "sha256:candidate-lock".to_string(),
        manifest_hash: "sha256:candidate-manifest".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "support".to_string(),
        requested_by_type: None,
        requested_by_id: None,
        source_kind: "test".to_string(),
        source_ref: None,
        origin_node_id: "gateway".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Candidate,
        metadata_json: serde_json::json!({}),
        short_id: String::new(),
        detected_network_hosts: None,
        signature: None,
        signer_id: None,
    };
    store.insert_agent_revision(&rec)?;
    // Deliberately NO alias — the candidate is not installed.
    Ok(REVISION_ID.to_string())
}

fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        if path.is_dir() {
            copy_dir_all(&path, &dst.join(file_name))?;
        } else {
            std::fs::copy(&path, dst.join(file_name))?;
        }
    }
    Ok(())
}

fn caller_manifest_with_spawn() -> autonoetic_types::agent::AgentManifest {
    let yaml = r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "caller.test"
  name: "caller.test"
  description: "Spawn-capable caller for io contract tests"
capabilities:
  - type: "AgentSpawn"
    max_children: 5
    max_spawn_depth: 2
llm_config:
  provider: "openai"
  model: "test-model"
---
# Caller
"#;
    let (manifest, _instructions) =
        autonoetic_gateway::runtime::parser::SkillParser::parse(yaml).unwrap();
    manifest
}

fn is_sandbox_unavailable(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    msg.contains("bwrap") || msg.contains("bubblewrap") || msg.contains("sandbox")
}

#[tokio::test]
async fn candidate_smoke_spawn_fails_on_io_returns_violation() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let agent_dir = workspace.agents_dir.join(VIOLATING_AGENT);
    // Script output is missing the required `status` field — the exact shape
    // of the weather-forecast incident.
    write_script_agent_dir(&agent_dir, VIOLATING_AGENT, r#"{"city": "Paris", "forecast": []}"#)?;

    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let revision_id = seed_candidate_revision(&store, &config, VIOLATING_AGENT, &agent_dir)?;

    let execution = GatewayExecutionService::new(config, Some(store));
    let result = execution
        .spawn_agent_revision_once(
            VIOLATING_AGENT,
            Some(&revision_id),
            r#"{"city": "Paris"}"#,
            "session-candidate-violating",
            None,
            false,
            None,
            None,
            None,
            None,
            None,
            &[],
        )
        .await;

    match result {
        Ok(spawn) => {
            panic!(
                "candidate smoke spawn must fail io.returns validation (missing 'status'); got reply: {:?}",
                spawn.assistant_reply
            );
        }
        Err(e) => {
            if is_sandbox_unavailable(&e) {
                tracing::warn!("sandbox unavailable, skipping: {e}");
                return Ok(());
            }
            let msg = e.to_string();
            assert!(
                msg.contains("status"),
                "error must name the missing required field, got: {msg}"
            );
            assert!(
                !msg.contains("No checkpoint found"),
                "script agents must fail fast — never enter the LLM repair loop (respawn_from_checkpoint): {msg}"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn candidate_smoke_spawn_passes_when_output_conforms() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let agent_dir = workspace.agents_dir.join(CONFORMING_AGENT);
    write_script_agent_dir(
        &agent_dir,
        CONFORMING_AGENT,
        r#"{"status": "success", "forecast": []}"#,
    )?;

    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let revision_id = seed_candidate_revision(&store, &config, CONFORMING_AGENT, &agent_dir)?;

    let execution = GatewayExecutionService::new(config, Some(store));
    let result = execution
        .spawn_agent_revision_once(
            CONFORMING_AGENT,
            Some(&revision_id),
            r#"{"city": "Paris"}"#,
            "session-candidate-conforming",
            None,
            false,
            None,
            None,
            None,
            None,
            None,
            &[],
        )
        .await;

    match result {
        Ok(spawn) => {
            let reply = spawn.assistant_reply.expect("reply present");
            let parsed: serde_json::Value = serde_json::from_str(&reply)?;
            assert_eq!(parsed["status"], "success");
        }
        Err(e) => {
            if is_sandbox_unavailable(&e) {
                tracing::warn!("sandbox unavailable, skipping: {e}");
                return Ok(());
            }
            return Err(e);
        }
    }
    Ok(())
}

#[tokio::test]
async fn spawn_input_validation_reads_candidate_revision_manifest() -> anyhow::Result<()> {
    // Gap 2b: `agent.spawn` with `revision_id` must enforce the CANDIDATE's
    // io.accepts — read from the pinned revision dir, not the live agent dir
    // (which does not exist for an unpromoted candidate). A plain-text message
    // must be rejected with the structured schema error before any execution.
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let agent_dir = workspace.agents_dir.join(VIOLATING_AGENT);
    write_script_agent_dir(&agent_dir, VIOLATING_AGENT, r#"{"status": "ok"}"#)?;
    // Remove the live dir entirely: the candidate exists only as a revision.
    std::fs::remove_dir_all(&agent_dir)?;

    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let staging = workspace.agents_dir.join("staging").join(VIOLATING_AGENT);
    write_script_agent_dir(&staging, VIOLATING_AGENT, r#"{"status": "ok"}"#)?;
    let revision_id = seed_candidate_revision(&store, &config, VIOLATING_AGENT, &staging)?;

    let caller_manifest = caller_manifest_with_spawn();
    let policy = PolicyEngine::new(caller_manifest.clone());
    let tool = AgentSpawnTool;
    let args = serde_json::json!({
        "agent_id": VIOLATING_AGENT,
        "revision_id": revision_id,
        "message": "Paris",
        "session_id": "session-spawn-input-check",
    });

    let out = tool.execute(
        &caller_manifest,
        &policy,
        &workspace.agents_dir.join("caller.test"),
        Some(&gateway_dir),
        &args.to_string(),
        Some("session-spawn-input-check"),
        None,
        Some(&config),
        Some(store),
        None,
    )?;
    let parsed: serde_json::Value = serde_json::from_str(&out)?;
    assert_eq!(
        parsed["ok"], false,
        "plain-text message must be rejected against the candidate's io.accepts: {parsed}"
    );
    assert_eq!(parsed["error"], "schema_validation_failed");
    assert!(
        parsed["expected_schema"].is_object(),
        "expected_schema from the candidate revision manifest must be surfaced: {parsed}"
    );
    Ok(())
}
