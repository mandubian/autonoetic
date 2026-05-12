//! Track B invariant pin (#185): pure-skill agents install via intent-only
//! artifact bundles.
//!
//! The Track B SKILL orchestration (agent-factory Step 2a → auditor →
//! specialized_builder) depends on two gateway-side invariants:
//!
//!   1. `artifact_build(kind = AgentBundle)` accepts a single `.skill.md`
//!      input with no entrypoints — i.e. an intent-only bundle.
//!   2. `agent_revision_create_from_intent` accepts an `artifact_ref`
//!      alongside `execution_mode = reasoning` and produces a revision
//!      keyed on that artifact_id (so `promotion_record` and R++2
//!      capability-delta gating both work uniformly).
//!
//! Neither of these is a *new* capability — `artifact_build` already
//! supports `kind = AgentBundle`, and the intent-create tool already
//! accepts an optional `artifact_ref`. This test pins both, so future
//! refactors cannot regress the pure-skill install path.
//!
//! Refs: docs/design/sealed-network-evaluation-plan.md §3.5.4 / scope 5.10.

mod support;

use autonoetic_gateway::artifact_store::ArtifactStore;
use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::tools::{AgentRevisionCreateFromIntentTool, NativeTool};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::artifact::ArtifactKind;
use autonoetic_types::capability::Capability;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;

fn manifest_with_capabilities(caps: Vec<Capability>) -> AgentManifest {
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
        serde_json::to_string(&json!(caps)).unwrap()
    );
    let (manifest, _instructions) =
        autonoetic_gateway::runtime::parser::SkillParser::parse(&yaml).unwrap();
    manifest
}

/// Intent-only SKILL body that agent-factory Step 2a would compose for a
/// pure-reasoning agent. No code references, no script_entry — the body
/// itself is the agent's executable contract.
const INTENT_ONLY_SKILL_BODY: &str = r#"# my-summarizer

You are a summarization agent. Given a body of text, produce a concise
summary that captures the key points in three to five sentences. Do not
add commentary. Do not invent facts not present in the source.
"#;

/// Build an intent-only artifact bundle the way agent-factory Step 2a
/// would: write the SKILL body as named content, then `artifact_build`
/// with kind = AgentBundle, no entrypoints, just the SKILL body input.
fn build_intent_only_artifact(
    gateway_dir: &Path,
    session_id: &str,
) -> autonoetic_types::artifact::ArtifactBundle {
    let content_store = ContentStore::new(gateway_dir).unwrap();
    let artifact_store = ArtifactStore::new(gateway_dir).unwrap();

    let handle = content_store.write(INTENT_ONLY_SKILL_BODY.as_bytes()).unwrap();
    content_store
        .register_name(session_id, "my-summarizer.skill.md", &handle)
        .unwrap();

    artifact_store
        .build_with_kind(
            &["my-summarizer.skill.md".to_string()],
            None, // no entrypoints — intent-only bundle
            None, // no layers
            ArtifactKind::AgentBundle,
            session_id,
        )
        .unwrap()
}

#[test]
fn intent_only_artifact_build_succeeds_with_only_skill_body() {
    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let bundle = build_intent_only_artifact(&gateway_dir, "build-session");

    assert!(
        bundle.artifact_id.starts_with("art_"),
        "intent-only bundle must produce a normal art_* ID: got {}",
        bundle.artifact_id
    );
    assert_eq!(
        bundle.files.len(),
        1,
        "intent-only bundle must contain exactly the one SKILL body file: got {} files",
        bundle.files.len()
    );
    assert!(
        bundle.files[0].name.ends_with(".skill.md"),
        "the one file in an intent-only bundle must be the SKILL body: got '{}'",
        bundle.files[0].name
    );
    assert!(
        bundle.entrypoints.is_empty(),
        "intent-only bundle must declare no entrypoints (it has no executable code): got {:?}",
        bundle.entrypoints
    );
    assert_eq!(
        bundle.kind,
        ArtifactKind::AgentBundle,
        "intent-only bundle must be tagged kind = AgentBundle: got {:?}",
        bundle.kind
    );
}

#[test]
fn intent_only_artifact_inspect_round_trips() {
    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let built = build_intent_only_artifact(&gateway_dir, "inspect-session");

    // The auditor's Shape-2 detection uses artifact_inspect to determine
    // there is no script_entry and only a SKILL body. Pin that the
    // inspected bundle preserves these properties.
    let artifact_store = ArtifactStore::new(&gateway_dir).unwrap();
    let inspected = artifact_store
        .inspect(&built.artifact_id)
        .expect("intent-only bundle must round-trip through inspect");

    assert_eq!(inspected.artifact_id, built.artifact_id);
    assert_eq!(inspected.files.len(), 1);
    assert!(inspected.files[0].name.ends_with(".skill.md"));
    assert!(inspected.entrypoints.is_empty());
    assert_eq!(inspected.kind, ArtifactKind::AgentBundle);
}

#[test]
fn revision_create_from_intent_accepts_artifact_ref_for_reasoning_mode() {
    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let bundle = build_intent_only_artifact(&gateway_dir, "revision-session");

    // Orchestrator manifest (the agent calling agent_revision_create_from_intent
    // — in practice this is specialized_builder.default).
    let orchestrator = manifest_with_capabilities(vec![Capability::AgentRevision {
        patterns: vec!["my-summarizer*".into()],
    }]);
    let policy = PolicyEngine::new(orchestrator.clone());
    let tool = AgentRevisionCreateFromIntentTool;

    let response = tool
        .execute(
            &orchestrator,
            &policy,
            Path::new("/tmp"),
            Some(&gateway_dir),
            &json!({
                "agent_id": "my-summarizer",
                "artifact_id": bundle.artifact_id,
                "instructions": INTENT_ONLY_SKILL_BODY,
                "description": "Summarization agent (intent-only install).",
                "execution_mode": "reasoning",
                "capabilities": [
                    {"type": "ReadAccess", "scopes": ["self.*"]},
                    {"type": "WriteAccess", "scopes": ["self.*"]},
                ],
                "llm_config": {
                    "provider": "openai",
                    "model": "test-model",
                },
            })
            .to_string(),
            Some("revision-session"),
            None,
            None,
            Some(store.clone()),
            None,
        )
        .expect("revision creation must produce a structured response");

    let parsed: serde_json::Value = serde_json::from_str(&response)
        .expect("revision_create_from_intent must return JSON");

    // The tool either succeeded outright or returned a structured error
    // envelope. For Track B we need the success path — the combination
    // (execution_mode = reasoning, artifact_ref present, no script_entry)
    // must not be rejected by the gateway.
    if parsed.get("ok") == Some(&serde_json::Value::Bool(false)) {
        panic!(
            "intent-only reasoning install must not be rejected by the gateway; \
             got error envelope: {}",
            response
        );
    }

    let revision_id = parsed
        .get("revision_id")
        .and_then(|v| v.as_str())
        .expect("successful intent-create must return revision_id");
    assert!(
        !revision_id.is_empty(),
        "revision_id must be a non-empty string"
    );

    // The revision must record the artifact_id we passed — this is the
    // identity invariant Track B relies on (promotion_record and
    // R++2 both key on artifact_id).
    let stored_artifact = parsed
        .get("artifact_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(
        stored_artifact, bundle.artifact_id,
        "revision must pin the intent-only artifact_id we built: stored {:?}, built {:?}",
        stored_artifact, bundle.artifact_id
    );
}

#[test]
fn intent_only_install_path_is_observably_distinct_from_script_install() {
    // Sanity pin: an intent-only bundle has *no* script_entry; a
    // hypothetical code-bearing bundle would. The auditor's Shape-2
    // discrimination relies on this — without it, the audit protocol
    // cannot route by artifact shape.
    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let intent_only = build_intent_only_artifact(&gateway_dir, "shape-session");

    // A separate code-bearing bundle in the same store.
    let content_store = ContentStore::new(&gateway_dir).unwrap();
    let artifact_store = ArtifactStore::new(&gateway_dir).unwrap();
    let py_handle = content_store.write(b"print('hello')").unwrap();
    content_store
        .register_name("shape-session", "main.py", &py_handle)
        .unwrap();
    let code_bearing = artifact_store
        .build_with_kind(
            &["main.py".to_string()],
            Some(&["main.py".to_string()]),
            None,
            ArtifactKind::AgentBundle,
            "shape-session",
        )
        .unwrap();

    assert!(
        intent_only.entrypoints.is_empty(),
        "intent-only: must have no entrypoints"
    );
    assert!(
        !code_bearing.entrypoints.is_empty(),
        "code-bearing: must have at least one entrypoint"
    );
    assert!(
        intent_only
            .files
            .iter()
            .all(|f| f.name.ends_with(".skill.md")),
        "intent-only: every file must be a SKILL body"
    );
    assert!(
        code_bearing
            .files
            .iter()
            .any(|f| !f.name.ends_with(".skill.md")),
        "code-bearing: must contain at least one non-SKILL file"
    );

    // The two artifacts must have *different* content-addressed IDs —
    // the same agent_id installed via the two paths produces two
    // distinct revisions in the audit trail.
    assert_ne!(
        intent_only.artifact_id, code_bearing.artifact_id,
        "intent-only and code-bearing bundles must hash to different artifact IDs"
    );
}
