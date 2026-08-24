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
        runtime_dir: gateway_dir.clone(),
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
        runtime_dir: gateway_dir.clone(),
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
        runtime_dir: gateway_dir.clone(),
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

// ── Replay-mode checkpoint history at the capsule boundary (#987) ───────────
//
// A Replay capsule embeds `SessionCheckpoint`, whose `history` is every tool
// result verbatim — the most sensitive payload a capsule can carry, and the one
// nothing filtered. The `memory_snapshot` beside it was already label-gated,
// which made the capsule *look* label-aware while the larger hole stayed open.

fn replay_checkpoint(
    session_id: &str,
    agent_id: &str,
    egress_labels: std::collections::HashMap<String, EgressLabel>,
) -> autonoetic_gateway::runtime::checkpoint::SessionCheckpoint {
    use autonoetic_gateway::llm::Message;
    autonoetic_gateway::runtime::checkpoint::SessionCheckpoint {
        egress_labels,
        egress_ask: None,
        history: vec![
            Message::system("You are a test agent"),
            Message::user("summarize my mail"),
            // Stands in for a labeled tool result: if the capsule ships, this
            // string ships with it.
            Message::assistant("CANARY-PRIVATE-HISTORY"),
        ],
        turn_counter: 1,
        session_state: Default::default(),
        tool_tier_escalated: false,
        session_phase: Default::default(),
        discovered_tools: Default::default(),
        blocked_state_event_emitted: false,
        extended_loaded: false,
        loop_guard_state: Default::default(),
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        turn_id: "turn-000001".to_string(),
        workflow_id: None,
        task_id: None,
        runtime_lock_hash: Some("abc123hash".to_string()),
        constitution_version: None,
        constitution_digest: None,
        llm_config_snapshot: None,
        tool_registry_version: None,
        yield_reason: autonoetic_gateway::runtime::checkpoint::YieldReason::Hibernation,
        content_store_refs: vec![],
        created_at: "2026-01-01T00:00:00Z".to_string(),
        pending_tool_state: None,
        llm_rounds_consumed: 1,
        tool_invocations_consumed: 0,
        tokens_consumed: 100,
        estimated_cost_usd: 0.001,
        compression_metadata: None,
        capsule_state: None,
        assistant_message: None,
        pending_action: None,
        suspended_at: None,
        suppress_until_turn: 0,
        trajectory_last_level: None,
        feedback_events: vec![],
    }
}

struct ReplayFixture {
    _tmp: tempfile::TempDir,
    gateway_dir: std::path::PathBuf,
    store: Arc<GatewayStore>,
    config: GatewayConfig,
    agent_id: String,
    revision_id: String,
    session_id: String,
}

fn replay_fixture() -> anyhow::Result<ReplayFixture> {
    let tmp = tempfile::tempdir()?;
    let agents_dir = tmp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let agent_id = "agent.capsule.replay".to_string();
    let revision_id = "rev_capsule_replay".to_string();
    seed_capsule_fixture(&gateway_dir, &store, &agent_id, &revision_id);
    let mut config = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir,
        ..Default::default()
    };
    config.capsule.auto_sign = false;
    Ok(ReplayFixture {
        _tmp: tmp,
        gateway_dir,
        store,
        config,
        agent_id,
        revision_id,
        session_id: "sess-replay".to_string(),
    })
}

fn export_replay(f: &ReplayFixture, trust_domain: &str) -> anyhow::Result<String> {
    let out_path = f
        .gateway_dir
        .parent()
        .unwrap()
        .join(format!("out-{trust_domain}.capsule.tar.zst"));
    let outcome = export(
        ExportRequest {
            agent_id: f.agent_id.clone(),
            revision_id: Some(f.revision_id.clone()),
            mode: CapsuleMode::Replay,
            include_memory: Some(false),
            sign: Some(false),
            output_path: Some(out_path),
            session_id: Some(f.session_id.clone()),
            root_session_id: None,
            destination_sink: None,
            trust_domain: Some(trust_domain.to_string()),
        },
        ExportContext {
            gateway_dir: &f.gateway_dir,
            gateway_config: &f.config,
            gateway_store: &f.store,
        },
    )?;
    Ok(outcome.capsule_path.display().to_string())
}

/// The hole: a session tainted `local_only` must not ship its history to a
/// partner (federated) destination.
#[test]
fn replay_export_refuses_when_session_taint_excludes_the_destination() -> anyhow::Result<()> {
    let f = replay_fixture()?;
    autonoetic_gateway::runtime::checkpoint::save_checkpoint(
        &f.config,
        &replay_checkpoint(&f.session_id, &f.agent_id, Default::default()),
    )?;
    f.store
        .restrict_session_egress_taint(&f.session_id, &EgressLabel::local_only())?;

    let err = export_replay(&f, "partner").expect_err("must refuse");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("capsule_replay_checkpoint_egress_refused"),
        "refusal should name the gate: {msg}"
    );
    assert!(
        msg.contains("local_only") && msg.contains("federated_agent"),
        "refusal should name the label and the sink: {msg}"
    );
    // …and it must say how to proceed, not just say no.
    assert!(
        msg.contains("thin or hermetic") || msg.contains("declassify"),
        "refusal should offer a path forward: {msg}"
    );
    Ok(())
}

/// The checkpoint's own label sidecar is authoritative about what its bytes
/// contain, and can outlive the session taint row (a forked or restored session
/// carries the sidecar, not the row). A clean taint row must not license a
/// history full of labeled results.
#[test]
fn replay_export_refuses_on_the_checkpoint_sidecar_alone() -> anyhow::Result<()> {
    let f = replay_fixture()?;
    let mut labels = std::collections::HashMap::new();
    labels.insert("tc_mail_1".to_string(), EgressLabel::local_only());
    autonoetic_gateway::runtime::checkpoint::save_checkpoint(
        &f.config,
        &replay_checkpoint(&f.session_id, &f.agent_id, labels),
    )?;
    // No `restrict_session_egress_taint` call: the taint row says nothing.

    let err = export_replay(&f, "partner").expect_err("sidecar alone must refuse");
    assert!(
        format!("{err:#}").contains("capsule_replay_checkpoint_egress_refused"),
        "the sidecar must gate even with a clean session taint row"
    );
    Ok(())
}

/// Not over-refusing: a `local` trust domain is a cleared sink for `local_only`,
/// so the same tainted session exports fine — and the canary really is in there,
/// which is what makes the refusal above matter.
#[test]
fn replay_export_allows_local_destination_for_a_tainted_session() -> anyhow::Result<()> {
    let f = replay_fixture()?;
    autonoetic_gateway::runtime::checkpoint::save_checkpoint(
        &f.config,
        &replay_checkpoint(&f.session_id, &f.agent_id, Default::default()),
    )?;
    f.store
        .restrict_session_egress_taint(&f.session_id, &EgressLabel::local_only())?;

    let path = export_replay(&f, "local").expect("local destination is cleared for local_only");

    let extract = tempfile::tempdir()?;
    autonoetic_gateway::capsule::archive::unpack(
        std::path::Path::new(&path),
        extract.path(),
        f.config.capsule.max_capsule_size_bytes,
    )?;
    let bytes = autonoetic_gateway::capsule::archive::read_entry(
        extract.path(),
        autonoetic_gateway::capsule::paths::CHECKPOINT_PATH,
    )?;
    assert!(
        String::from_utf8_lossy(&bytes).contains("CANARY-PRIVATE-HISTORY"),
        "the checkpoint really does carry history verbatim — which is why the \
         partner-destination refusal matters"
    );
    Ok(())
}

/// A clean session is unaffected: no taint, no sidecar labels, ships anywhere.
#[test]
fn replay_export_allows_a_clean_session_to_a_partner() -> anyhow::Result<()> {
    let f = replay_fixture()?;
    autonoetic_gateway::runtime::checkpoint::save_checkpoint(
        &f.config,
        &replay_checkpoint(&f.session_id, &f.agent_id, Default::default()),
    )?;
    export_replay(&f, "partner").expect("a clean session has nothing to withhold");
    Ok(())
}

/// The refusal is auditable: `egress.boundary_refused` lands with the capsule
/// surface, so "why did my export fail?" is answerable from the chain.
#[test]
fn replay_refusal_emits_boundary_refused_for_the_capsule_surface() -> anyhow::Result<()> {
    let f = replay_fixture()?;
    autonoetic_gateway::runtime::checkpoint::save_checkpoint(
        &f.config,
        &replay_checkpoint(&f.session_id, &f.agent_id, Default::default()),
    )?;
    f.store
        .restrict_session_egress_taint(&f.session_id, &EgressLabel::local_only())?;
    let _ = export_replay(&f, "partner").expect_err("must refuse");

    let events = f
        .store
        .search_causal_events(Some(&f.session_id), None, 50)?
        .into_iter()
        .filter(|e| e.action == "egress.boundary_refused")
        .collect::<Vec<_>>();
    let ev = events.first().expect("boundary_refused emitted");
    let payload: serde_json::Value =
        serde_json::from_str(ev.payload.as_deref().unwrap_or("{}"))?;
    assert_eq!(payload["surface"], "capsule");
    assert_eq!(payload["label_name"], "local_only");
    assert_eq!(
        payload["reason"], "capsule_replay_checkpoint_egress_refused",
        "the reason must identify this gate, not a generic refusal"
    );
    // Content-free: the canary must never reach an audit event.
    assert!(
        !ev.payload.as_deref().unwrap_or("").contains("CANARY"),
        "audit events are metadata only"
    );
    Ok(())
}
