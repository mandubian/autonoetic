//! Gateway-owned host contract — install rejection + NULL grandfathering.

use autonoetic_gateway::runtime::tools::agent_revision::AgentRevisionCreateFromIntentTool;
use autonoetic_gateway::runtime::tools::NativeTool;
use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, ExecutionMode, ScriptInputMode};
use autonoetic_types::artifact::{ArtifactRefRecord, ArtifactRefScopeType};
use autonoetic_types::capability::Capability;
use autonoetic_types::agent_revision::AgentRevisionRecord;
use serial_test::serial;
use std::sync::Arc;
use tempfile::tempdir;

fn builder_manifest() -> AgentManifest {
    AgentManifest {
        version: "1.0".to_string(),
        runtime: autonoetic_gateway::runtime::install_contract::default_runtime_declaration(),
        agent: AgentIdentity {
            id: "specialized-builder.test".to_string(),
            name: "specialized-builder.test".to_string(),
            description: "test".to_string(),
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
        agentskills_import: None,
        compression: None,
        open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn seed_weather_artifact(
    gateway_dir: &std::path::Path,
    session_id: &str,
) -> (String, String) {
    let code = b"#!/usr/bin/env python3\nGEOCODING='https://geocoding-api.open-meteo.com/v1/search'\nFORECAST='https://api.open-meteo.com/v1/forecast'\n";
    let content_store =
        autonoetic_gateway::runtime::content_store::ContentStore::new(gateway_dir).unwrap();
    let handle = content_store.write(code).unwrap();
    content_store
        .register_name(session_id, "weather.py", &handle)
        .unwrap();

    let artifact_store = autonoetic_gateway::artifact_store::ArtifactStore::new(gateway_dir).unwrap();
    let bundle = artifact_store
        .build(
            &["weather.py".to_string()],
            Some(&["weather.py".to_string()]),
            None,
            session_id,
        )
        .unwrap();
    let artifact_ref = "ar.hostcontract01".to_string();
    (artifact_ref, bundle.artifact_id)
}

#[test]
#[serial]
fn host_contract_rejects_wildcard_with_detected_hosts() {
    let dir = tempdir().unwrap();
    let gateway_dir = dir.path().join(".gateway");
    let session_id = "sess-host-contract-wildcard";

    let (artifact_ref, artifact_id) = seed_weather_artifact(&gateway_dir, session_id);
    let gateway_store = Arc::new(
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
    );
    gateway_store
        .create_artifact_ref(&ArtifactRefRecord {
            ref_id: artifact_ref.clone(),
            scope_type: ArtifactRefScopeType::Session,
            scope_id: session_id.to_string(),
            artifact_id,
            artifact_manifest_digest: "digest".to_string(),
            artifact_canonical_digest: "canonical".to_string(),
            created_by_agent_id: "specialized-builder.test".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            expires_at: None,
            revoked_at: None,
        })
        .unwrap();

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
                "agent_id": "weather-agent",
                "artifact_ref": artifact_ref,
                "description": "Weather",
                "instructions": "# Weather",
                "execution_mode": "script",
                "script_entry": "weather.py",
                "capabilities": [
                    {"type": "NetworkAccess", "hosts": ["*"]}
                ]
            })
            .to_string(),
            Some(session_id),
            None,
            None,
            Some(gateway_store),
            None,
        )
        .unwrap();

    let response_json: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response_json.get("ok").and_then(|v| v.as_bool()), Some(false));
    let suggested = response_json
        .get("suggested_hosts")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        suggested.iter().any(|h| h.as_str().unwrap_or("").contains("open-meteo")),
        "expected suggested_hosts to include open-meteo hosts: {response_json}"
    );
}

#[test]
#[serial]
fn host_contract_persists_detected_hosts_on_success() {
    let dir = tempdir().unwrap();
    let gateway_dir = dir.path().join(".gateway");
    let session_id = "sess-host-contract-persist";

    let (artifact_ref, artifact_id) = seed_weather_artifact(&gateway_dir, session_id);
    let gateway_store = Arc::new(
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
    );
    gateway_store
        .create_artifact_ref(&ArtifactRefRecord {
            ref_id: artifact_ref.clone(),
            scope_type: ArtifactRefScopeType::Session,
            scope_id: session_id.to_string(),
            artifact_id,
            artifact_manifest_digest: "digest".to_string(),
            artifact_canonical_digest: "canonical".to_string(),
            created_by_agent_id: "specialized-builder.test".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            expires_at: None,
            revoked_at: None,
        })
        .unwrap();

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
                "agent_id": "weather-agent-ok",
                "artifact_ref": artifact_ref,
                "description": "Weather",
                "instructions": "# Weather",
                "execution_mode": "script",
                "script_entry": "weather.py",
                "capabilities": [
                    {"type": "NetworkAccess", "hosts": ["api.open-meteo.com", "geocoding-api.open-meteo.com"]}
                ]
            })
            .to_string(),
            Some(session_id),
            None,
            None,
            Some(gateway_store.clone()),
            None,
        )
        .unwrap();

    let response_json: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response_json.get("ok").and_then(|v| v.as_bool()), Some(true));
    let revision_id = response_json
        .get("revision_id")
        .and_then(|v| v.as_str())
        .unwrap();
    let revision = gateway_store.get_agent_revision(revision_id).unwrap().unwrap();
    let hosts = revision.detected_network_hosts.unwrap_or_default();
    assert!(hosts.iter().any(|h| h.contains("api.open-meteo.com")));
    assert!(hosts.iter().any(|h| h.contains("geocoding-api.open-meteo.com")));
}

#[test]
#[serial]
fn null_detected_hosts_grandfathered_on_read() {
    let dir = tempdir().unwrap();
    let gateway_dir = dir.path().join(".gateway");
    let store =
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();

    let revision_id = "rev_sha256:legacy-null-hosts".to_string();
    store
        .insert_agent_revision(&AgentRevisionRecord {
            revision_id: revision_id.clone(),
            agent_id: "legacy-agent".to_string(),
            base_revision_id: None,
            artifact_id: None,
            content_digest: "sha256:legacy".to_string(),
            runtime_lock_hash: "sha256:lock".to_string(),
            manifest_hash: "sha256:manifest".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            created_by_type: "test".to_string(),
            created_by_id: "test".to_string(),
            source_kind: "test".to_string(),
            source_ref: None,
            origin_node_id: "gateway".to_string(),
            trust_domain: "local".to_string(),
            status: autonoetic_types::agent_revision::AgentRevisionStatus::Candidate,
            metadata_json: serde_json::json!({}),
            short_id: String::new(),
            signature: None,
            signer_id: None,
            detected_network_hosts: None,
        })
        .unwrap();

    let loaded = store.get_agent_revision(&revision_id).unwrap().unwrap();
    assert!(loaded.detected_network_hosts.is_none());
}

#[test]
#[serial]
fn migration_v56_adds_detected_network_hosts_column() {
    let dir = tempdir().unwrap();
    let gateway_dir = dir.path().join(".gateway");
    let store =
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
    drop(store);

    let conn = rusqlite::Connection::open(gateway_dir.join("gateway.db")).unwrap();
    let col_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('agent_revisions') WHERE name = 'detected_network_hosts'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(col_count, 1);

    let version: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(version >= 56);
}
