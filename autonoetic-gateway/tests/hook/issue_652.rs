//! Regression for #652: `agent_revision_create_from_intent` with `replace: true`
//! must NOT archive the existing Ready revision at create time. `atomic_promote`
//! archives the outgoing revision atomically at promote time; archiving earlier
//! leaves the alias pointing at an `Archived` revision with no `Ready` backing
//! if the candidate is never promoted (smoke-test failure / operator reject /
//! eval gate / abandoned run).

use autonoetic_gateway::artifact_store::ArtifactStore;
use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::tools::{AgentRevisionCreateFromIntentTool, NativeTool};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::agent_revision::{AgentRevisionRecord, AgentRevisionStatus};
use autonoetic_types::artifact::ArtifactKind;
use autonoetic_types::capability::Capability;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;

fn orchestrator_manifest(agent_id: &str) -> AgentManifest {
    let yaml = format!(
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
  id: "test-orchestrator"
  name: "Test Orchestrator"
  description: "Test agent for invariant pins."
capabilities: {}
llm_config:
  provider: "openai"
  model: "test-model"
---
# Test Orchestrator
"#,
        serde_json::to_string(&json!([Capability::AgentRevision {
            patterns: vec![format!("{}*", agent_id)],
        }]))
        .unwrap()
    );
    let (manifest, _instructions) =
        autonoetic_gateway::runtime::parser::SkillParser::parse(&yaml).unwrap();
    manifest
}

/// Build an intent-only AgentBundle from a SKILL body. Distinct bodies yield
/// distinct content-addressed artifact_ids (and thus distinct revision_ids).
fn build_intent_artifact(
    gateway_dir: &Path,
    session_id: &str,
    skill_name: &str,
    skill_body: &str,
) -> String {
    let content_store = ContentStore::new(gateway_dir).unwrap();
    let artifact_store = ArtifactStore::new(gateway_dir).unwrap();
    let handle = content_store.write(skill_body.as_bytes()).unwrap();
    content_store
        .register_name(session_id, skill_name, &handle)
        .unwrap();
    artifact_store
        .build_with_kind(
            &[skill_name.to_string()],
            None,
            None,
            ArtifactKind::AgentBundle,
            session_id,
        )
        .unwrap()
        .artifact_id
}

fn create_revision(
    tool: &AgentRevisionCreateFromIntentTool,
    manifest: &AgentManifest,
    policy: &PolicyEngine,
    gateway_dir: &Path,
    store: &Arc<GatewayStore>,
    agent_id: &str,
    artifact_id: &str,
    skill_body: &str,
    replace: bool,
) -> String {
    let mut args = json!({
        "agent_id": agent_id,
        "artifact_id": artifact_id,
        "instructions": skill_body,
        "description": "test agent",
        "execution_mode": "reasoning",
        "capabilities": [{"type": "ReadAccess", "scopes": ["self.*"]}],
        "llm_config": {"provider": "openai", "model": "test-model"},
    });
    if replace {
        args["replace"] = json!(true);
    }
    let response = tool
        .execute(
            manifest,
            policy,
            Path::new("/tmp"),
            Some(gateway_dir),
            &args.to_string(),
            Some("revision-session"),
            None,
            None,
            Some(store.clone()),
            None,
        )
        .expect("create_from_intent must produce a structured response");
    let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(
        parsed["ok"],
        serde_json::Value::Bool(true),
        "create_from_intent failed: {response}"
    );
    parsed["revision_id"].as_str().unwrap().to_string()
}

fn status_of(revisions: &[AgentRevisionRecord], revision_id: &str) -> AgentRevisionStatus {
    revisions
        .iter()
        .find(|r| r.revision_id == revision_id)
        .unwrap_or_else(|| panic!("revision {revision_id} should be stored"))
        .status
        .clone()
}

#[test]
fn replace_true_does_not_archive_active_ready_revision() {
    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let agent_id = "my-replace-agent";
    let manifest = orchestrator_manifest(agent_id);
    let policy = PolicyEngine::new(manifest.clone());
    let tool = AgentRevisionCreateFromIntentTool;

    // R1: create + promote so it is the active Ready revision behind the alias.
    let artifact_r1 = build_intent_artifact(&gateway_dir, "s1", "r1.skill.md", "# R1\nv1 body\n");
    let r1 = create_revision(
        &tool,
        &manifest,
        &policy,
        &gateway_dir,
        &store,
        agent_id,
        &artifact_r1,
        "# R1\nv1 body\n",
        false,
    );
    store
        .atomic_promote(agent_id, &r1, "promo-1", "test", "test", None, None, None)
        .unwrap();

    // Sanity: R1 is Ready and the alias points at it.
    let r1_status = status_of(&store.list_agent_revisions(agent_id).unwrap(), &r1);
    assert_eq!(r1_status, AgentRevisionStatus::Ready, "R1 should be Ready after promote");
    assert_eq!(
        store.resolve_alias(agent_id).unwrap().unwrap().revision_id,
        r1,
        "alias must point at R1"
    );

    // R2: create a distinct candidate with replace:true. Pre-fix, this archived
    // R1 eagerly; post-fix R1 must stay Ready and the alias must not move.
    let artifact_r2 = build_intent_artifact(&gateway_dir, "s2", "r2.skill.md", "# R2\nv2 body\n");
    let r2 = create_revision(
        &tool,
        &manifest,
        &policy,
        &gateway_dir,
        &store,
        agent_id,
        &artifact_r2,
        "# R2\nv2 body\n",
        true,
    );
    assert_ne!(r2, r1, "R2 must be a distinct revision");

    let r1_status_after = status_of(&store.list_agent_revisions(agent_id).unwrap(), &r1);
    assert_eq!(
        r1_status_after,
        AgentRevisionStatus::Ready,
        "R1 must NOT be archived at create(replace:true) time — issue #652"
    );
    assert_eq!(
        store.resolve_alias(agent_id).unwrap().unwrap().revision_id,
        r1,
        "alias must still point at the live Ready revision until R2 is promoted"
    );
    // R2 is a Candidate awaiting smoke-test + promote.
    let r2_status_after = status_of(&store.list_agent_revisions(agent_id).unwrap(), &r2);
    assert_eq!(r2_status_after, AgentRevisionStatus::Candidate);
}
