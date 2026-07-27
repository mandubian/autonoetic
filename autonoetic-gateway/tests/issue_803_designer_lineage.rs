//! #803 — designer lineage on factory-built revisions. `created_by_*` records
//! the installer (always `specialized_builder.default` by capability
//! isolation); `requested_by_*` must record the delegating principal (e.g.
//! `agent-factory.default`), derived by the gateway from the calling session's
//! spawn lineage — never from LLM-supplied arguments.

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::agent_revision::AgentRevisionCreateFromIntentTool;
use autonoetic_gateway::runtime::tools::NativeTool;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, ExecutionMode, ScriptInputMode};
use autonoetic_types::artifact::{ArtifactRefRecord, ArtifactRefScopeType};
use autonoetic_types::capability::Capability;
use serial_test::serial;
use std::sync::Arc;
use tempfile::tempdir;

fn builder_manifest() -> AgentManifest {
    AgentManifest {
        version: "1.0".to_string(),
        runtime: autonoetic_gateway::runtime::install_contract::default_runtime_declaration(),
        agent: AgentIdentity {
            id: "specialized_builder.default".to_string(),
            name: "specialized_builder.default".to_string(),
            description: "test installer".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::AgentRevision {
            patterns: vec!["*".to_string()],
        }],
        llm_preset: None,
        llm_overrides: None,
        llm_config: None,
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        execution_mode: ExecutionMode::Reasoning,
        script_entry: None,
        script_input_mode: ScriptInputMode::default(),
        gateway_url: None,
        gateway_token: None,
        allowed_tool_tiers: vec![],
        excluded_tools: vec![],
        agentskills_import: None,
        compression: None,
        open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn seed_artifact(
    gateway_dir: &std::path::Path,
    gateway_store: &autonoetic_gateway::scheduler::gateway_store::GatewayStore,
    session_id: &str,
    artifact_ref: &str,
) {
    let code = b"#!/usr/bin/env python3\nprint('ok')\n";
    let content_store =
        autonoetic_gateway::runtime::content_store::ContentStore::new(gateway_dir).unwrap();
    let handle = content_store.write(code).unwrap();
    content_store
        .register_name(session_id, "main.py", &handle)
        .unwrap();

    let artifact_store =
        autonoetic_gateway::artifact_store::ArtifactStore::new(gateway_dir).unwrap();
    let bundle = artifact_store
        .build(
            &["main.py".to_string()],
            Some(&["main.py".to_string()]),
            None,
            session_id,
        )
        .unwrap();

    gateway_store
        .create_artifact_ref(&ArtifactRefRecord {
            ref_id: artifact_ref.to_string(),
            scope_type: ArtifactRefScopeType::Session,
            scope_id: session_id.to_string(),
            artifact_id: bundle.artifact_id,
            artifact_manifest_digest: "digest".to_string(),
            artifact_canonical_digest: "canonical".to_string(),
            created_by_agent_id: "specialized_builder.default".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            expires_at: None,
            revoked_at: None,
        })
        .unwrap();
}

fn create_revision(
    dir: &std::path::Path,
    gateway_dir: &std::path::Path,
    gateway_store: Arc<autonoetic_gateway::scheduler::gateway_store::GatewayStore>,
    session_id: Option<&str>,
    agent_id: &str,
    artifact_ref: &str,
) -> serde_json::Value {
    let manifest = builder_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let tool = AgentRevisionCreateFromIntentTool;
    let response = tool
        .execute(
            &manifest,
            &policy,
            dir,
            Some(gateway_dir),
            &serde_json::json!({
                "agent_id": agent_id,
                "artifact_ref": artifact_ref,
                "description": "lineage test agent",
                "instructions": "# Lineage",
                "execution_mode": "script",
                "script_entry": "main.py",
                "capabilities": []
            })
            .to_string(),
            session_id,
            None,
            None,
            Some(gateway_store),
            None,
        )
        .unwrap();
    serde_json::from_str(&response).unwrap()
}

/// The full factory path: root spawns agent-factory, agent-factory spawns the
/// builder, the builder creates the revision. The revision must name
/// agent-factory as requester while created_by stays the installer.
#[test]
#[serial]
fn designer_lineage_recorded_from_spawn_chain() {
    let dir = tempdir().unwrap();
    let gateway_dir = dir.path().join(".gateway");
    let gateway_store = Arc::new(
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
    );

    gateway_store
        .upsert_session_spawn_lineage(
            "root/factory-1",
            "root",
            "root",
            2,
            "agent-factory.default",
            "2026-07-01T00:00:00Z",
        )
        .unwrap();
    gateway_store
        .upsert_session_spawn_lineage(
            "root/factory-1/builder-2",
            "root/factory-1",
            "root",
            4,
            "specialized_builder.default",
            "2026-07-01T00:00:01Z",
        )
        .unwrap();

    let session_id = "root/factory-1/builder-2";
    seed_artifact(&gateway_dir, &gateway_store, session_id, "ar.lineage0001");

    let out = create_revision(
        dir.path(),
        &gateway_dir,
        gateway_store.clone(),
        Some(session_id),
        "lineage-agent-factory",
        "ar.lineage0001",
    );
    assert_eq!(out.get("ok").and_then(|v| v.as_bool()), Some(true), "{out}");
    let revision_id = out.get("revision_id").and_then(|v| v.as_str()).unwrap();

    let revision = gateway_store
        .get_agent_revision(revision_id)
        .unwrap()
        .expect("revision persisted");
    assert_eq!(revision.created_by_id, "specialized_builder.default");
    assert_eq!(
        revision.requested_by_type.as_deref(),
        Some("autonoetic_agent")
    );
    assert_eq!(
        revision.requested_by_id.as_deref(),
        Some("agent-factory.default")
    );
}

/// Installer invoked directly by the operator (no parent session): the
/// requester is underivable and must stay `None` rather than falling back to
/// the installer or any tool argument.
#[test]
#[serial]
fn designer_lineage_none_when_installer_invoked_at_root() {
    let dir = tempdir().unwrap();
    let gateway_dir = dir.path().join(".gateway");
    let gateway_store = Arc::new(
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
    );

    let session_id = "root";
    seed_artifact(&gateway_dir, &gateway_store, session_id, "ar.lineage0002");

    let out = create_revision(
        dir.path(),
        &gateway_dir,
        gateway_store.clone(),
        Some(session_id),
        "lineage-agent-root",
        "ar.lineage0002",
    );
    assert_eq!(out.get("ok").and_then(|v| v.as_bool()), Some(true), "{out}");
    let revision_id = out.get("revision_id").and_then(|v| v.as_str()).unwrap();

    let revision = gateway_store
        .get_agent_revision(revision_id)
        .unwrap()
        .expect("revision persisted");
    assert_eq!(revision.created_by_id, "specialized_builder.default");
    assert!(revision.requested_by_type.is_none());
    assert!(revision.requested_by_id.is_none());
}

/// Even if the LLM passes `requested_by_*` arguments, they must be ignored —
/// lineage is gateway-derived only.
#[test]
#[serial]
fn designer_lineage_ignores_llm_supplied_requester() {
    let dir = tempdir().unwrap();
    let gateway_dir = dir.path().join(".gateway");
    let gateway_store = Arc::new(
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
    );

    let session_id = "root";
    seed_artifact(&gateway_dir, &gateway_store, session_id, "ar.lineage0003");

    let manifest = builder_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let tool = AgentRevisionCreateFromIntentTool;
    let response = tool
        .execute(
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::json!({
                "agent_id": "lineage-agent-forged",
                "artifact_ref": "ar.lineage0003",
                "description": "forged lineage attempt",
                "instructions": "# Forge",
                "execution_mode": "script",
                "script_entry": "main.py",
                "capabilities": [],
                "requested_by_type": "human",
                "requested_by_id": "operator"
            })
            .to_string(),
            Some(session_id),
            None,
            None,
            Some(gateway_store.clone()),
            None,
        )
        .unwrap();
    let out: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(out.get("ok").and_then(|v| v.as_bool()), Some(true), "{out}");
    let revision_id = out.get("revision_id").and_then(|v| v.as_str()).unwrap();

    let revision = gateway_store
        .get_agent_revision(revision_id)
        .unwrap()
        .expect("revision persisted");
    assert!(
        revision.requested_by_type.is_none() && revision.requested_by_id.is_none(),
        "LLM-supplied requester must not be persisted: {revision:?}"
    );
}
