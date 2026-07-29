//! Phase 4 (#909) slice 5: capsule export label filtering by destination sink.

use std::sync::Arc;

use autonoetic_gateway::capsule::{export, ExportContext, ExportRequest};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent_revision::{
    AgentAliasRecord, AgentRevisionRecord, AgentRevisionStatus,
};
use autonoetic_types::capsule::CapsuleMode;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::egress::{EgressLabel, NamedEgressLabel, Sink};
use autonoetic_types::memory::MemoryObject;
use autonoetic_types::principal::PrincipalKind;

fn seed_capsule_fixture(
    gateway_dir: &std::path::Path,
    store: &GatewayStore,
    agent_id: &str,
    revision_id: &str,
) {
    let rev_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join(agent_id)
        .join(revision_id);
    std::fs::create_dir_all(&rev_dir).unwrap();
    std::fs::write(rev_dir.join("SKILL.md"), "# test\n").unwrap();
    std::fs::write(rev_dir.join("runtime.lock"), "agents: []\n").unwrap();

    let rev = AgentRevisionRecord {
        revision_id: revision_id.to_string(),
        agent_id: agent_id.to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest: format!("sha256:{}", "a".repeat(64)),
        runtime_lock_hash: format!("sha256:{}", "b".repeat(64)),
        manifest_hash: format!("sha256:{}", "c".repeat(64)),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "test".to_string(),
        requested_by_type: None,
        requested_by_id: None,
        source_kind: "artifact".to_string(),
        source_ref: None,
        origin_node_id: "node-A".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Ready,
        metadata_json: serde_json::Value::Null,
        short_id: "abcd1234".to_string(),
        detected_network_hosts: None,
        signature: None,
        signer_id: None,
    };
    store.insert_agent_revision(&rev).unwrap();
    store
        .upsert_agent_alias(&AgentAliasRecord {
            alias_id: agent_id.to_string(),
            agent_id: agent_id.to_string(),
            revision_id: revision_id.to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            updated_by_type: PrincipalKind::Human.tag().to_string(),
            updated_by_id: "test".to_string(),
            reason: None,
            suspended_at: None,
            suspended_reason: None,
            suspended_by: None,
        })
        .unwrap();
}

#[test]
fn capsule_export_withholds_local_only_memory_for_partner_destination() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let agents_dir = tmp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let agent_id = "agent.capsule";
    let revision_id = "rev_capsule_test";
    seed_capsule_fixture(&gateway_dir, &store, agent_id, revision_id);

    let mut allowed = MemoryObject::new(
        "mem-allowed".into(),
        "memory".into(),
        agent_id.into(),
        agent_id.into(),
        "sess".into(),
        "public fact".into(),
    );
    allowed.egress_label = Some(EgressLabel::unrestricted());

    let mut secret = MemoryObject::new(
        "mem-secret".into(),
        "memory".into(),
        agent_id.into(),
        agent_id.into(),
        "sess".into(),
        "private fact".into(),
    );
    secret.egress_label = Some(EgressLabel::local_only());

    store.memory_upsert(&allowed)?;
    store.memory_upsert(&secret)?;

    let out_path = tmp.path().join("out.capsule.tar.zst");
    let mut config = GatewayConfig {
        agents_dir,
        ..Default::default()
    };
    config.capsule.auto_sign = false;

    let outcome = export(
        ExportRequest {
            agent_id: agent_id.to_string(),
            revision_id: Some(revision_id.to_string()),
            mode: CapsuleMode::Thin,
            include_memory: Some(true),
            sign: Some(false),
            output_path: Some(out_path),
            session_id: None,
            root_session_id: None,
            destination_sink: None,
            trust_domain: Some("partner".to_string()),
        },
        ExportContext {
            gateway_dir: &gateway_dir,
            gateway_config: &config,
            gateway_store: &store,
        },
    )?;

    assert_eq!(outcome.destination_sink, "federated_agent");
    assert_eq!(outcome.memory_withheld_count, 1);

    let extract = tempfile::tempdir()?;
    autonoetic_gateway::capsule::archive::unpack(
        &outcome.capsule_path,
        extract.path(),
        config.capsule.max_capsule_size_bytes,
    )?;
    let manifest_bytes = autonoetic_gateway::capsule::archive::read_entry(extract.path(), "capsule.json")?;
    let manifest: autonoetic_types::capsule::CapsuleManifest = serde_json::from_slice(&manifest_bytes)?;
    let snap = manifest
        .memory_snapshot
        .as_ref()
        .expect("memory snapshot metadata");
    assert_eq!(snap.entry_count, 1);
    assert_eq!(snap.withheld_count, 1);
    assert_eq!(manifest.provenance.memory_withheld_count, 1);
    assert_eq!(
        manifest.provenance.destination_sink.as_deref(),
        Some("federated_agent")
    );
    Ok(())
}

#[test]
fn capsule_export_includes_local_only_memory_for_local_destination() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let agents_dir = tmp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let agent_id = "agent.local";
    let revision_id = "rev_local";
    seed_capsule_fixture(&gateway_dir, &store, agent_id, revision_id);

    let mut mem = MemoryObject::new(
        "mem-local".into(),
        "memory".into(),
        agent_id.into(),
        agent_id.into(),
        "sess".into(),
        "local-only content".into(),
    );
    mem.egress_label = Some(EgressLabel::local_only());
    store.memory_upsert(&mem)?;

    let out_path = tmp.path().join("local.capsule.tar.zst");
    let mut config = GatewayConfig {
        agents_dir,
        ..Default::default()
    };
    config.capsule.auto_sign = false;

    let outcome = export(
        ExportRequest {
            agent_id: agent_id.to_string(),
            revision_id: Some(revision_id.to_string()),
            mode: CapsuleMode::Thin,
            include_memory: Some(true),
            sign: Some(false),
            output_path: Some(out_path),
            session_id: None,
            root_session_id: None,
            destination_sink: Some(Sink::LocalAgent),
            trust_domain: None,
        },
        ExportContext {
            gateway_dir: &gateway_dir,
            gateway_config: &config,
            gateway_store: &store,
        },
    )?;

    assert_eq!(outcome.memory_withheld_count, 0);
    assert_eq!(outcome.destination_sink, "local_agent");

    let extract = tempfile::tempdir()?;
    autonoetic_gateway::capsule::archive::unpack(
        &outcome.capsule_path,
        extract.path(),
        config.capsule.max_capsule_size_bytes,
    )?;
    let manifest_bytes = autonoetic_gateway::capsule::archive::read_entry(extract.path(), "capsule.json")?;
    let manifest: autonoetic_types::capsule::CapsuleManifest = serde_json::from_slice(&manifest_bytes)?;
    assert_eq!(
        manifest.memory_snapshot.as_ref().map(|s| s.entry_count),
        Some(1)
    );
    Ok(())
}

#[test]
fn capsule_export_legacy_unlabeled_memory_respects_config() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let agents_dir = tmp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let agent_id = "agent.legacy";
    let revision_id = "rev_legacy";
    seed_capsule_fixture(&gateway_dir, &store, agent_id, revision_id);

    let mut mem = MemoryObject::new(
        "mem-legacy".into(),
        "memory".into(),
        agent_id.into(),
        agent_id.into(),
        "sess".into(),
        "legacy row".into(),
    );
    mem.egress_label = None;
    store.memory_upsert(&mem)?;

    let out_path = tmp.path().join("legacy.capsule.tar.zst");
    let mut config = GatewayConfig {
        agents_dir,
        egress: autonoetic_types::egress::EgressConfig {
            legacy_unlabeled: NamedEgressLabel::NoRemoteModel,
            ..Default::default()
        },
        ..Default::default()
    };
    config.capsule.auto_sign = false;

    let outcome = export(
        ExportRequest {
            agent_id: agent_id.to_string(),
            revision_id: Some(revision_id.to_string()),
            mode: CapsuleMode::Thin,
            include_memory: Some(true),
            sign: Some(false),
            output_path: Some(out_path),
            session_id: None,
            root_session_id: None,
            destination_sink: Some(Sink::RemoteModel),
            trust_domain: None,
        },
        ExportContext {
            gateway_dir: &gateway_dir,
            gateway_config: &config,
            gateway_store: &store,
        },
    )?;

    assert_eq!(outcome.memory_withheld_count, 1);
    Ok(())
}
