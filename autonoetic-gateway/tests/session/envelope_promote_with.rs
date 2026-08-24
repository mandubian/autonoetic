//! Promotion pre-authorization via locked session envelope PromoteWith (#503).

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::session_envelope::lock_session_envelope;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use autonoetic_types::agent_revision::{AgentRevisionRecord, AgentRevisionStatus};
use autonoetic_types::background::ScheduledAction;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::principal::PrincipalKind;
use std::sync::Arc;
use tempfile::tempdir;
use crate::support::manifest_builder::TestManifest;

const AGENT_ID: &str = "weather.forecast";
const INCOMING_REVISION: &str = "rev_incoming";

fn manifest_with_capabilities(caps: Vec<Capability>) -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: AGENT_ID.to_string(),
            name: AGENT_ID.to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: caps,
        ..TestManifest::new().build()
    }
}

fn skill_md(capabilities_yaml: &str) -> String {
    format!(
        "---\nversion: \"1.0\"\nruntime:\n  engine: autonoetic\n  gateway_version: \"0.1.0\"\n  sdk_version: \"0.1.0\"\n  type: stateful\n  sandbox: bubblewrap\n  runtime_lock: runtime.lock\nagent:\n  id: {}\n  name: {}\n  description: test\n{}\n---\n# Test\n",
        AGENT_ID, AGENT_ID, capabilities_yaml,
    )
}

fn write_revision_skill(gateway_dir: &std::path::Path, revision_id: &str, capabilities_yaml: &str) {
    let dir = gateway_dir
        .join("revisions/agents")
        .join(AGENT_ID)
        .join(revision_id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), skill_md(capabilities_yaml)).unwrap();
}

fn make_revision_record(revision_id: &str) -> AgentRevisionRecord {
    AgentRevisionRecord {
        revision_id: revision_id.to_string(),
        agent_id: AGENT_ID.to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest: format!("sha256:{}", revision_id),
        runtime_lock_hash: "sha256:lock".to_string(),
        manifest_hash: "sha256:manifest".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "test".to_string(),
        requested_by_type: None,
        requested_by_id: None,
        source_kind: "artifact".to_string(),
        source_ref: None,
        origin_node_id: "local".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Candidate,
        metadata_json: serde_json::Value::Null,
        short_id: revision_id.chars().take(8).collect(),
        detected_network_hosts: None,
        signature: None,
        signer_id: None,
    }
}

struct PromoteHarness {
    _temp: tempfile::TempDir,
    store: Arc<GatewayStore>,
    agent_dir: std::path::PathBuf,
    gateway_dir: std::path::PathBuf,
    root_session: String,
}

fn setup_new_agent_harness(incoming_caps_yaml: &str) -> PromoteHarness {
    let temp = tempdir().expect("tempdir");
    let agents_dir = temp.path().join("agents");
    let agent_dir = agents_dir.join(AGENT_ID);
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).expect("store opens"));

    write_revision_skill(&gateway_dir, INCOMING_REVISION, incoming_caps_yaml);
    store
        .insert_agent_revision(&make_revision_record(INCOMING_REVISION))
        .unwrap();

    PromoteHarness {
        _temp: temp,
        store,
        agent_dir,
        gateway_dir,
        root_session: "root-promote-preauth".to_string(),
    }
}

fn lock_promote_with(
    store: &GatewayStore,
    root_session: &str,
    agent_id: &str,
    capabilities: Vec<Capability>,
) -> i64 {
    let now = chrono::Utc::now().to_rfc3339();
    let promote_with = Capability::PromoteWith {
        agent_id: agent_id.to_string(),
        capabilities,
    };
    let envelope_id = store
        .insert_envelope_proposal(root_session, &promote_with, "test", Some(&now), None, &now)
        .unwrap();
    lock_session_envelope(store, envelope_id, "operator").unwrap();
    envelope_id
}

fn lock_promote_with_at(
    store: &GatewayStore,
    root_session: &str,
    agent_id: &str,
    capabilities: Vec<Capability>,
    created_at: &str,
) -> i64 {
    let promote_with = Capability::PromoteWith {
        agent_id: agent_id.to_string(),
        capabilities,
    };
    let envelope_id = store
        .insert_envelope_proposal(root_session, &promote_with, "test", Some(created_at), None, created_at)
        .unwrap();
    lock_session_envelope(store, envelope_id, "operator").unwrap();
    envelope_id
}

fn invoke_promote(h: &PromoteHarness, args_json: &str) -> serde_json::Value {
    let manifest = manifest_with_capabilities(vec![Capability::AgentRevision {
        patterns: vec!["*".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let config = GatewayConfig {
        agents_dir: h
            .agent_dir
            .parent()
            .expect("agent dir parent")
            .to_path_buf(),
        // Must match the gateway dir handed to the tool below: the
        // capability-delta check resolves the outgoing revision through
        // `gateway_root_dir(config)`, so a mismatch silently finds no outgoing
        // revision and reports no delta.
        runtime_dir: h.gateway_dir.clone(),
        ..GatewayConfig::default()
    };
    let session_id = format!("{}/promoter", h.root_session);
    let raw = registry
        .execute(
            "agent_revision_promote",
            &manifest,
            &policy,
            &h.agent_dir,
            Some(&h.gateway_dir),
            args_json,
            Some(&session_id),
            Some("turn-000001"),
            Some(&config),
            Some(h.store.clone()),
            None,
        )
        .expect("execute should not error for normal cases");
    serde_json::from_str(&raw).expect("response is JSON")
}

fn revision_promote_approvals(store: &GatewayStore) -> usize {
    store
        .get_pending_approvals()
        .expect("pending approvals")
        .into_iter()
        .filter(|a| matches!(a.action, ScheduledAction::RevisionPromote { .. }))
        .count()
}

#[test]
fn locked_promote_with_skips_capability_ack_approval() {
    let incoming_caps = "capabilities:\n  - type: NetworkAccess\n    hosts: [\"api.open-meteo.com\"]\n  - type: ReadAccess\n    scopes: [\"self.*\"]";
    let h = setup_new_agent_harness(incoming_caps);

    let envelope_id = lock_promote_with(
        &h.store,
        &h.root_session,
        AGENT_ID,
        vec![
            Capability::NetworkAccess {
                hosts: vec!["api.open-meteo.com".to_string()],
            },
            Capability::ReadAccess {
                scopes: vec!["self.*".to_string()],
            },
        ],
    );

    let resp = invoke_promote(
        &h,
        &format!(r#"{{"agent_id":"{AGENT_ID}","revision_id":"{INCOMING_REVISION}"}}"#),
    );
    assert_ne!(
        resp["error"].as_str(),
        Some("capability_delta_requires_approval"),
        "PromoteWith should pre-authorize promotion: {resp}"
    );
    assert_eq!(
        revision_promote_approvals(&h.store),
        0,
        "no RevisionPromote approval should be created when pre-authorized"
    );
    assert_eq!(
        autonoetic_gateway::runtime::session_envelope::find_promote_with_envelope_id(
            &h.store,
            &h.root_session,
            AGENT_ID,
            &[
                Capability::NetworkAccess {
                    hosts: vec!["api.open-meteo.com".to_string()],
                },
                Capability::ReadAccess {
                    scopes: vec!["self.*".to_string()],
                },
            ],
        )
        .expect("envelope lookup"),
        Some(envelope_id),
        "locked PromoteWith should be traceable to envelope ID"
    );
}

#[test]
fn promote_with_extra_capability_still_requires_ack() {
    let incoming_caps = "capabilities:\n  - type: NetworkAccess\n    hosts: [\"api.open-meteo.com\"]\n  - type: ReadAccess\n    scopes: [\"self.*\"]\n  - type: WriteAccess\n    scopes: [\"self.*\"]";
    let h = setup_new_agent_harness(incoming_caps);

    lock_promote_with(
        &h.store,
        &h.root_session,
        AGENT_ID,
        vec![
            Capability::NetworkAccess {
                hosts: vec!["api.open-meteo.com".to_string()],
            },
            Capability::ReadAccess {
                scopes: vec!["self.*".to_string()],
            },
        ],
    );

    let resp = invoke_promote(
        &h,
        &format!(r#"{{"agent_id":"{AGENT_ID}","revision_id":"{INCOMING_REVISION}"}}"#),
    );
    assert_eq!(resp["error"], "capability_delta_requires_approval");
    assert_eq!(resp["approval_required"], true);
    assert_eq!(revision_promote_approvals(&h.store), 1);
}

#[test]
fn promote_with_agent_id_mismatch_does_not_apply() {
    let incoming_caps = "capabilities:\n  - type: NetworkAccess\n    hosts: [\"api.open-meteo.com\"]";
    let h = setup_new_agent_harness(incoming_caps);

    lock_promote_with(
        &h.store,
        &h.root_session,
        "other.agent",
        vec![Capability::NetworkAccess {
            hosts: vec!["api.open-meteo.com".to_string()],
        }],
    );

    let resp = invoke_promote(
        &h,
        &format!(r#"{{"agent_id":"{AGENT_ID}","revision_id":"{INCOMING_REVISION}"}}"#),
    );
    assert_eq!(resp["error"], "capability_delta_requires_approval");
    assert_eq!(revision_promote_approvals(&h.store), 1);
}

#[test]
fn any_matching_locked_promote_with_preauthorizes() {
    let incoming_caps = "capabilities:\n  - type: NetworkAccess\n    hosts: [\"api.open-meteo.com\"]\n  - type: ReadAccess\n    scopes: [\"self.*\"]";
    let h = setup_new_agent_harness(incoming_caps);

    // Newer agent-specific entry is too narrow (network only).
    lock_promote_with_at(
        &h.store,
        &h.root_session,
        AGENT_ID,
        vec![Capability::NetworkAccess {
            hosts: vec!["api.open-meteo.com".to_string()],
        }],
        "2026-06-15T10:00:00Z",
    );
    // Older wildcard entry covers the full artifact capability set.
    lock_promote_with_at(
        &h.store,
        &h.root_session,
        "",
        vec![
            Capability::NetworkAccess {
                hosts: vec!["api.open-meteo.com".to_string()],
            },
            Capability::ReadAccess {
                scopes: vec!["self.*".to_string()],
            },
        ],
        "2026-06-15T09:00:00Z",
    );

    let resp = invoke_promote(
        &h,
        &format!(r#"{{"agent_id":"{AGENT_ID}","revision_id":"{INCOMING_REVISION}"}}"#),
    );
    assert_ne!(
        resp["error"].as_str(),
        Some("capability_delta_requires_approval"),
        "any matching PromoteWith should pre-authorize: {resp}"
    );
    assert_eq!(revision_promote_approvals(&h.store), 0);
}

#[test]
fn artifact_build_proposes_promote_with_from_skill_md() -> anyhow::Result<()> {
    use autonoetic_gateway::artifact_store::ArtifactStore;
    use autonoetic_gateway::runtime::content_store::ContentStore;
    use autonoetic_gateway::runtime::session_envelope::propose_envelopes_after_artifact_build;
    use autonoetic_types::artifact::ArtifactKind;

    let dir = tempdir()?;
    let gateway_dir = dir.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = GatewayStore::open(&gateway_dir)?;
    let content_store = ContentStore::new(&gateway_dir)?;
    let artifact_store = ArtifactStore::new(&gateway_dir)?;
    let session_id = "root-artifact-promote/agent";

    let skill = skill_md(
        "capabilities:\n  - type: NetworkAccess\n    hosts: [\"api.open-meteo.com\"]\n  - type: CodeExecution\n    patterns: [\"python*\"]",
    );
    let handle = content_store.write(skill.as_bytes())?;
    content_store
        .register_name(session_id, "SKILL.md", &handle)?;

    let bundle = artifact_store.build_with_kind(
        &["SKILL.md".to_string()],
        None,
        None,
        ArtifactKind::AgentBundle,
        session_id,
    )?;

    let root = "root-artifact-promote";
    propose_envelopes_after_artifact_build(
        &store,
        &gateway_dir,
        root,
        &bundle.artifact_id,
        &ArtifactKind::AgentBundle,
        "coder.default",
    )?;

    let proposed = store.get_proposed_envelopes(root)?;
    assert!(
        proposed.iter().any(|p| matches!(
            &p.capability,
            Capability::PromoteWith { agent_id, capabilities }
                if agent_id == AGENT_ID
                    && capabilities.iter().any(|c| matches!(c, Capability::NetworkAccess { .. }))
                    && capabilities.iter().any(|c| matches!(c, Capability::CodeExecution { .. }))
        )),
        "expected PromoteWith proposal from agent_bundle SKILL.md: {:?}",
        proposed
    );
    Ok(())
}
