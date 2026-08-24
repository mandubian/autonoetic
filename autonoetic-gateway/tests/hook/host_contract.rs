//! Gateway-owned host contract — install rejection + NULL grandfathering.


use autonoetic_gateway::runtime::tools::agent_revision::AgentRevisionCreateFromIntentTool;
use autonoetic_gateway::runtime::tools::NativeTool;
use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, ExecutionMode};
use autonoetic_types::artifact::{ArtifactRefRecord, ArtifactRefScopeType};
use autonoetic_types::capability::Capability;
use autonoetic_types::agent_revision::AgentRevisionRecord;
use serial_test::serial;
use std::sync::Arc;
use tempfile::tempdir;
use crate::support::manifest_builder::TestManifest;

fn builder_manifest() -> AgentManifest {
    AgentManifest {
        runtime: autonoetic_gateway::runtime::install_contract::default_runtime_declaration(),
        agent: AgentIdentity {
            id: "specialized-builder.test".to_string(),
            name: "specialized-builder.test".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::AgentRevision {
            patterns: vec!["*".to_string()],
        }],
        execution_mode: ExecutionMode::Reasoning,
        ..TestManifest::new().build()
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
                ],
                "io": {
                    "accepts": {"type": "object", "required": ["city"], "properties": {"city": {"type": "string"}}}
                }
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
            requested_by_type: None,
            requested_by_id: None,
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

#[test]
#[serial]
fn grant_drift_emits_causal_event_for_host_outside_contract() {
    use autonoetic_gateway::scheduler::approval::{apply_decision, DecisionContext};
    use autonoetic_types::agent_revision::{
        AgentAliasRecord, AgentRevisionStatus, SessionAgentBinding,
    };
    use autonoetic_types::background::{
        ApprovalDecision, ApprovalLevel, ApprovalStatus, ScheduledAction,
    };
    use autonoetic_types::config::GatewayConfig;
    use autonoetic_types::principal::PrincipalKind;

    let dir = tempdir().unwrap();
    let agents_dir = dir.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store =
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();

    let agent_id = "weather-agent-drift";
    let revision_id = "rev_sha256:drift-test".to_string();
    let session_id = "session-drift/coder.default-abc".to_string();
    let root_session_id = "session-drift".to_string();

    store
        .insert_agent_revision(&AgentRevisionRecord {
            revision_id: revision_id.clone(),
            agent_id: agent_id.to_string(),
            base_revision_id: None,
            artifact_id: None,
            content_digest: "sha256:drift".to_string(),
            runtime_lock_hash: "sha256:lock".to_string(),
            manifest_hash: "sha256:manifest".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            created_by_type: PrincipalKind::Human.tag().to_string(),
            created_by_id: "test".to_string(),
            requested_by_type: None,
            requested_by_id: None,
            source_kind: "test".to_string(),
            source_ref: None,
            origin_node_id: "gateway".to_string(),
            trust_domain: "local".to_string(),
            status: AgentRevisionStatus::Ready,
            metadata_json: serde_json::json!({}),
            short_id: String::new(),
            signature: None,
            signer_id: None,
            detected_network_hosts: Some(vec!["api.open-meteo.com".to_string()]),
        })
        .unwrap();

    store
        .upsert_agent_alias(&AgentAliasRecord {
            alias_id: agent_id.to_string(),
            agent_id: agent_id.to_string(),
            revision_id: revision_id.clone(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            updated_by_type: PrincipalKind::Human.tag().to_string(),
            updated_by_id: "test".to_string(),
            reason: Some("test".to_string()),
            suspended_at: None,
            suspended_reason: None,
            suspended_by: None,
        })
        .unwrap();

    store
        .upsert_session_agent_binding(&SessionAgentBinding {
            session_id: session_id.clone(),
            root_session_id: root_session_id.clone(),
            alias_id: Some(agent_id.to_string()),
            agent_id: agent_id.to_string(),
            revision_id: revision_id.clone(),
            runtime_lock_hash: "sha256:lock".to_string(),
            constitution_version: None,
            constitution_digest: None,
            home_node_id: "gateway".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            requested_target: agent_id.to_string(),
        })
        .unwrap();

    let cfg = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir,
        ..Default::default()
    };

    let decision = ApprovalDecision {
        request_id: "apr-drift01".to_string(),
        agent_id: agent_id.to_string(),
        session_id: session_id.clone(),
        action: ScheduledAction::SandboxExec {
            command: "curl https://evil.com".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: Some(vec!["evil.com".to_string()]),
            intent: None,
        },
        status: ApprovalStatus::Approved,
        decided_at: chrono::Utc::now().to_rfc3339(),
        decided_by: "operator".to_string(),
        reason: None,
        workflow_id: None,
        task_id: None,
        root_session_id: Some(root_session_id),
        approval_level: ApprovalLevel::Operator,
    };

    apply_decision(
        &cfg,
        Some(&store),
        &decision,
        &Default::default(),
        &DecisionContext {
            wiki_materialized_meta: None,
            hook_executor: None,
        },
    )
    .unwrap();

    let events = store
        .search_causal_events(Some(&session_id), Some(agent_id), 20)
        .unwrap();
    assert!(
        events.iter().any(|e| {
            e.category == "host_contract"
                && e.action == "host_outside_revision_contract"
                && e.target.as_deref() == Some("evil.com")
        }),
        "expected host_outside_revision_contract causal event, got: {events:?}"
    );
}
