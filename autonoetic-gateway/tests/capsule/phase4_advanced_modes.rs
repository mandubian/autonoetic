//! Phase 4 integration tests — Replay mode, Headless mode, memory
//! import dedup, and platform-mismatch refusal.

use autonoetic_gateway::capsule::{export, import, ExportContext, ImportContext, ExportRequest, ImportRequest};
use autonoetic_gateway::capsule::import::MemoryConflictPolicy;
use autonoetic_gateway::llm::Message;
use autonoetic_gateway::runtime::checkpoint::{
    list_checkpoints, save_checkpoint, SessionCheckpoint, YieldReason,
};
use autonoetic_gateway::runtime::guard::LoopGuard;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent_revision::{
    AgentAliasRecord, AgentRevisionRecord, AgentRevisionStatus,
};
use autonoetic_types::capsule::{CapsuleMode, CapsulePlatform};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::memory::{MemoryObject, MemoryVisibility};
use autonoetic_types::principal::PrincipalKind;
use autonoetic_types::scheduled_job::{ScheduledJob, ScheduledJobStatus};
use std::sync::Arc;
use tempfile::tempdir;

struct Fixture {
    _temp: tempfile::TempDir,
    config: GatewayConfig,
    store: Arc<GatewayStore>,
    gateway_dir: std::path::PathBuf,
}

fn fixture(agent_id: &str, revision_id: &str) -> Fixture {
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let rev_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join(agent_id)
        .join(revision_id);
    std::fs::create_dir_all(&rev_dir).unwrap();
    std::fs::write(rev_dir.join("SKILL.md"), "# agent\n").unwrap();
    std::fs::write(rev_dir.join("runtime.lock"), "agents: []\n").unwrap();

    let rev = AgentRevisionRecord {
        revision_id: revision_id.to_string(),
        agent_id: agent_id.to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest: format!("sha256:{}", "x".repeat(64)),
        runtime_lock_hash: format!("sha256:{}", "y".repeat(64)),
        manifest_hash: format!("sha256:{}", "z".repeat(64)),
        created_at: "2026-05-28T00:00:00Z".to_string(),
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
    let alias = AgentAliasRecord {
        alias_id: agent_id.to_string(),
        agent_id: agent_id.to_string(),
        revision_id: revision_id.to_string(),
        updated_at: "2026-05-28T00:00:00Z".to_string(),
        updated_by_type: PrincipalKind::Human.tag().to_string(),
        updated_by_id: "test".to_string(),
        reason: None,
        suspended_at: None,
        suspended_reason: None,
        suspended_by: None,
    };
    store.upsert_agent_alias(&alias).unwrap();

    let config = GatewayConfig {
        agents_dir,
        ..Default::default()
    };

    Fixture {
        _temp: temp,
        config,
        store,
        gateway_dir,
    }
}

fn fresh_fixture() -> Fixture {
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let config = GatewayConfig {
        agents_dir,
        ..Default::default()
    };
    Fixture {
        _temp: temp,
        config,
        store,
        gateway_dir,
    }
}

#[test]
fn memory_export_then_import_roundtrip_with_conflict_policy() {
    let f = fixture("mem.agent", "rev_sha256:mem-001");
    // Seed two memories owned by the agent.
    for (id, content) in [("mem-1", "fact one"), ("mem-2", "fact two")] {
        let now = "2026-05-28T00:00:00Z".to_string();
        let obj = MemoryObject {
            memory_id: id.to_string(),
            scope: "memory".to_string(),
            owner_agent_id: "mem.agent".to_string(),
            writer_agent_id: "mem.agent".to_string(),
            source_type: Default::default(),
            source_ref: "test".to_string(),
            created_at: now.clone(),
            updated_at: now,
            content: content.to_string(),
            content_hash: "sha256:0".to_string(),
            confidence: None,
            tags: vec![],
            lineage: vec![],
            visibility: MemoryVisibility::default(),
            expires_at: None,
            revision_id: None,
            binding_session_id: None,
            alias_ref: None,
            quarantine_reason: None,
            egress_label: None,
        };
        f.store.memory_upsert(&obj).unwrap();
    }

    let out_dir = tempdir().unwrap();
    let archive = out_dir.path().join("mem.capsule.tar.zst");
    let outcome = export(
        ExportRequest {
            agent_id: "mem.agent".to_string(),
            revision_id: None,
            mode: CapsuleMode::Thin,
            include_memory: Some(true),
            sign: Some(false),
            output_path: Some(archive.clone()),
            session_id: None,
            root_session_id: None,
            destination_sink: None,
            trust_domain: None,
        },
        ExportContext {
            gateway_dir: &f.gateway_dir,
            gateway_config: &f.config,
            gateway_store: &f.store,
        },
    )
    .expect("export");
    let _ = outcome;

    // Fresh receiver — first import lays both entries down.
    let r = fresh_fixture();
    let first = import(
        ImportRequest {
            archive_path: archive.clone(),
            verify_signature: false,
            dry_run: false,
            activate: false,
            trust_domain_override: None,
            memory_conflict_policy: MemoryConflictPolicy::KeepLocal,
        },
        ImportContext {
            gateway_dir: &r.gateway_dir,
            gateway_config: &r.config,
            gateway_store: &r.store,
        },
    )
    .expect("first import");
    assert_eq!(first.memory_entries_imported, 2);
    assert_eq!(first.memory_entries_skipped, 0);
    assert!(r.store.memory_get_unrestricted("mem-1").unwrap().is_some());

    // Second import with KeepLocal must skip both (already exist).
    let second = import(
        ImportRequest {
            archive_path: archive.clone(),
            verify_signature: false,
            dry_run: false,
            activate: false,
            trust_domain_override: None,
            memory_conflict_policy: MemoryConflictPolicy::KeepLocal,
        },
        ImportContext {
            gateway_dir: &r.gateway_dir,
            gateway_config: &r.config,
            gateway_store: &r.store,
        },
    )
    .expect("second import");
    assert_eq!(second.memory_entries_imported, 0);
    assert_eq!(second.memory_entries_skipped, 2);

    // Third import with OverwriteLocal must replace both.
    let third = import(
        ImportRequest {
            archive_path: archive,
            verify_signature: false,
            dry_run: false,
            activate: false,
            trust_domain_override: None,
            memory_conflict_policy: MemoryConflictPolicy::OverwriteLocal,
        },
        ImportContext {
            gateway_dir: &r.gateway_dir,
            gateway_config: &r.config,
            gateway_store: &r.store,
        },
    )
    .expect("third import");
    assert_eq!(third.memory_entries_imported, 2);
    assert_eq!(third.memory_entries_skipped, 0);
}

#[test]
fn replay_mode_bundles_checkpoint_and_import_lays_it_down() {
    let f = fixture("rep.agent", "rev_sha256:rep-001");
    // Persist a synthetic checkpoint for a session.
    let ckpt = SessionCheckpoint {
        egress_labels: Default::default(),
        history: vec![Message::system("test")],
        turn_counter: 7,
        session_state: Default::default(),
        tool_tier_escalated: false,
        discovered_tools: Default::default(),
        blocked_state_event_emitted: false,
        loop_guard_state: LoopGuard {
            max_loops_without_progress: 10,
            max_tool_failures: 5,
            max_consecutive_same_progress: 2,
            max_child_failures: 3,
            current_loops: 1,
            tool_failure_counts: std::collections::HashMap::new(),
            last_progress_fingerprint: None,
            consecutive_progress_count: 0,
            child_failure_count: 0,
            ..Default::default()
        },
        agent_id: "rep.agent".to_string(),
        session_id: "sess-1".to_string(),
        turn_id: "t007".to_string(),
        workflow_id: None,
        task_id: None,
        runtime_lock_hash: Some("abc".to_string()),
        constitution_version: None,
        constitution_digest: None,
        llm_config_snapshot: None,
        tool_registry_version: None,
        yield_reason: YieldReason::Hibernation,
        content_store_refs: vec![],
        created_at: "2026-05-28T00:00:00Z".to_string(),
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
    };
    save_checkpoint(&f.config, &ckpt).unwrap();

    let out_dir = tempdir().unwrap();
    let archive = out_dir.path().join("rep.capsule.tar.zst");
    export(
        ExportRequest {
            agent_id: "rep.agent".to_string(),
            revision_id: None,
            mode: CapsuleMode::Replay,
            include_memory: Some(false),
            sign: Some(false),
            output_path: Some(archive.clone()),
            session_id: Some("sess-1".to_string()),
            root_session_id: None,
            destination_sink: None,
            trust_domain: None,
        },
        ExportContext {
            gateway_dir: &f.gateway_dir,
            gateway_config: &f.config,
            gateway_store: &f.store,
        },
    )
    .expect("export");

    let r = fresh_fixture();
    let outcome = import(
        ImportRequest {
            archive_path: archive,
            verify_signature: false,
            dry_run: false,
            activate: false,
            trust_domain_override: None,
            memory_conflict_policy: MemoryConflictPolicy::KeepLocal,
        },
        ImportContext {
            gateway_dir: &r.gateway_dir,
            gateway_config: &r.config,
            gateway_store: &r.store,
        },
    )
    .expect("import");
    assert!(
        outcome.checkpoint_restored,
        "checkpoint should be restored on Replay-mode import"
    );

    // The receiver should now have a checkpoint at the same session_id +
    // turn_id ready for the scheduler to resume.
    let entries = list_checkpoints(&r.config, "sess-1").unwrap();
    assert!(
        entries.iter().any(|p| p.contains("t007")),
        "expected a t007 checkpoint, got {:?}",
        entries
    );
}

#[test]
fn headless_mode_bundles_and_recreates_scheduled_jobs() {
    let f = fixture("hl.agent", "rev_sha256:hl-001");
    let now = "2026-05-28T00:00:00Z".to_string();
    let job = ScheduledJob {
        job_id: "job-orig-001".to_string(),
        owner_agent_id: "hl.agent".to_string(),
        root_session_id: "root-1".to_string(),
        target_agent_id: "hl.agent".to_string(),
        target_revision_id: "rev_sha256:hl-001".to_string(),
        message: "tick".to_string(),
        metadata_json: None,
        cron_expr: "0 * * * *".to_string(),
        timezone: "UTC".to_string(),
        next_run_at: now.clone(),
        last_run_at: None,
        status: ScheduledJobStatus::Active,
        created_at: now.clone(),
        updated_at: now,
        last_error: None,
        generation: 0,
    };
    f.store.create_scheduled_job(&job).unwrap();

    let out_dir = tempdir().unwrap();
    let archive = out_dir.path().join("hl.capsule.tar.zst");
    export(
        ExportRequest {
            agent_id: "hl.agent".to_string(),
            revision_id: None,
            mode: CapsuleMode::Headless,
            include_memory: Some(false),
            sign: Some(false),
            output_path: Some(archive.clone()),
            session_id: None,
            root_session_id: Some("root-1".to_string()),
            destination_sink: None,
            trust_domain: None,
        },
        ExportContext {
            gateway_dir: &f.gateway_dir,
            gateway_config: &f.config,
            gateway_store: &f.store,
        },
    )
    .expect("export");

    let r = fresh_fixture();
    let outcome = import(
        ImportRequest {
            archive_path: archive,
            verify_signature: false,
            dry_run: false,
            activate: false,
            trust_domain_override: None,
            memory_conflict_policy: MemoryConflictPolicy::KeepLocal,
        },
        ImportContext {
            gateway_dir: &r.gateway_dir,
            gateway_config: &r.config,
            gateway_store: &r.store,
        },
    )
    .expect("import");
    assert_eq!(outcome.scheduled_jobs_recreated, 1);

    // The new job exists with a different (capsule-prefixed) job_id but
    // the same cron and owner.
    let owned = r
        .store
        .list_scheduled_jobs_for_owner("hl.agent", None, None)
        .unwrap();
    assert_eq!(owned.len(), 1);
    let new_job = &owned[0];
    assert!(
        new_job.job_id.starts_with("job_capsule_job-orig-001_"),
        "unexpected job_id: {}",
        new_job.job_id
    );
    assert_eq!(new_job.cron_expr, "0 * * * *");
    assert_eq!(new_job.target_agent_id, "hl.agent");
}

#[test]
fn platform_mismatch_refused_when_trust_domain_not_local() {
    let f = fixture("plat.agent", "rev_sha256:plat-001");

    let out_dir = tempdir().unwrap();
    let archive = out_dir.path().join("plat.capsule.tar.zst");
    export(
        ExportRequest {
            agent_id: "plat.agent".to_string(),
            revision_id: None,
            mode: CapsuleMode::Thin,
            include_memory: Some(false),
            sign: Some(false),
            output_path: Some(archive.clone()),
            session_id: None,
            root_session_id: None,
            destination_sink: None,
            trust_domain: None,
        },
        ExportContext {
            gateway_dir: &f.gateway_dir,
            gateway_config: &f.config,
            gateway_store: &f.store,
        },
    )
    .expect("export");

    // Manually tamper the platform in the embedded manifest to look
    // hostile. The signature was disabled so digest mismatch is moot.
    let work = tempdir().unwrap();
    autonoetic_gateway::capsule::archive::unpack(&archive, work.path(), 16 * 1024 * 1024).unwrap();
    let manifest_path = work.path().join("capsule.json");
    let mut m: autonoetic_types::capsule::CapsuleManifest =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    m.platform = Some(CapsulePlatform {
        os: "alien-os".to_string(),
        arch: "made-up-arch".to_string(),
    });
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&m).unwrap()).unwrap();
    autonoetic_gateway::capsule::archive::pack(work.path(), &archive).unwrap();

    let r = fresh_fixture();
    // trust_domain "foreign" + mismatched platform should refuse.
    let err = import(
        ImportRequest {
            archive_path: archive.clone(),
            verify_signature: false,
            dry_run: false,
            activate: false,
            trust_domain_override: Some("foreign".to_string()),
            memory_conflict_policy: MemoryConflictPolicy::KeepLocal,
        },
        ImportContext {
            gateway_dir: &r.gateway_dir,
            gateway_config: &r.config,
            gateway_store: &r.store,
        },
    )
    .expect_err("platform mismatch in foreign trust domain must refuse");
    assert!(
        err.to_string().contains("built for alien-os/made-up-arch"),
        "{}",
        err
    );

    // Same archive in trust_domain "local" should succeed (bypass for
    // operator-controlled imports).
    let r2 = fresh_fixture();
    let ok = import(
        ImportRequest {
            archive_path: archive,
            verify_signature: false,
            dry_run: false,
            activate: false,
            trust_domain_override: None, // defaults to "local"
            memory_conflict_policy: MemoryConflictPolicy::KeepLocal,
        },
        ImportContext {
            gateway_dir: &r2.gateway_dir,
            gateway_config: &r2.config,
            gateway_store: &r2.store,
        },
    )
    .expect("local trust domain bypasses platform check");
    assert!(ok.created_revision);
}

#[test]
fn import_refuses_memory_entry_with_mismatched_owner() {
    let f = fixture("own.agent", "rev_sha256:own-001");
    let now = "2026-05-28T00:00:00Z".to_string();
    // Seed one legitimate memory owned by the agent, plus one owned by
    // a different agent (which a tampered capsule could try to inject).
    for (id, owner) in [("good-1", "own.agent"), ("evil-2", "other.agent")] {
        let obj = MemoryObject {
            memory_id: id.to_string(),
            scope: "memory".to_string(),
            owner_agent_id: owner.to_string(),
            writer_agent_id: owner.to_string(),
            source_type: Default::default(),
            source_ref: "test".to_string(),
            created_at: now.clone(),
            updated_at: now.clone(),
            content: "x".to_string(),
            content_hash: "sha256:0".to_string(),
            confidence: None,
            tags: vec![],
            lineage: vec![],
            visibility: MemoryVisibility::default(),
            expires_at: None,
            revision_id: None,
            binding_session_id: None,
            alias_ref: None,
            quarantine_reason: None,
            egress_label: None,
        };
        f.store.memory_upsert(&obj).unwrap();
    }

    // Override `memory_list_ids_owned_by` semantics by exporting (which
    // only lists `owner_agent_id == agent_id`)…but to force the bad
    // entry into the bundle we hand-rewrite memory_snapshot.json after
    // unpacking the archive. This simulates a tampered capsule.
    let out_dir = tempdir().unwrap();
    let archive = out_dir.path().join("own.capsule.tar.zst");
    export(
        ExportRequest {
            agent_id: "own.agent".to_string(),
            revision_id: None,
            mode: CapsuleMode::Thin,
            include_memory: Some(true),
            sign: Some(false),
            output_path: Some(archive.clone()),
            session_id: None,
            root_session_id: None,
            destination_sink: None,
            trust_domain: None,
        },
        ExportContext {
            gateway_dir: &f.gateway_dir,
            gateway_config: &f.config,
            gateway_store: &f.store,
        },
    )
    .expect("export");

    // Tamper: inject an extra entry owned by `other.agent`.
    let work = tempdir().unwrap();
    autonoetic_gateway::capsule::archive::unpack(&archive, work.path(), 16 * 1024 * 1024).unwrap();
    let snapshot_path = work.path().join("memory/memory_snapshot.json");
    let mut snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&snapshot_path).unwrap()).unwrap();
    let injected = serde_json::json!({
        "memory_id": "evil-2",
        "scope": "memory",
        "owner_agent_id": "other.agent",
        "writer_agent_id": "other.agent",
        "source_type": "agent_write",
        "source_ref": "tampered",
        "created_at": "2026-05-28T00:00:00Z",
        "updated_at": "2026-05-28T00:00:00Z",
        "content": "evil",
        "content_hash": "sha256:0",
        "tags": [],
        "lineage": [],
        "visibility": "private",
        "expires_at": null,
        "revision_id": null,
        "binding_session_id": null,
        "alias_ref": null,
        "quarantine_reason": null,
    });
    snapshot["entries"]
        .as_array_mut()
        .unwrap()
        .push(injected);
    std::fs::write(&snapshot_path, serde_json::to_vec_pretty(&snapshot).unwrap()).unwrap();
    autonoetic_gateway::capsule::archive::pack(work.path(), &archive).unwrap();

    let r = fresh_fixture();
    let outcome = import(
        ImportRequest {
            archive_path: archive,
            verify_signature: false,
            dry_run: false,
            activate: false,
            trust_domain_override: None,
            memory_conflict_policy: MemoryConflictPolicy::KeepLocal,
        },
        ImportContext {
            gateway_dir: &r.gateway_dir,
            gateway_config: &r.config,
            gateway_store: &r.store,
        },
    )
    .expect("import");
    // The legitimate entry should be imported; the tampered entry must
    // be skipped (not silently injected into the receiver's store).
    assert_eq!(outcome.memory_entries_imported, 1);
    assert!(outcome.memory_entries_skipped >= 1);
    assert!(
        r.store.memory_get_unrestricted("good-1").unwrap().is_some(),
        "good-1 should be imported"
    );
    assert!(
        r.store.memory_get_unrestricted("evil-2").unwrap().is_none(),
        "evil-2 must NOT be imported despite being in the archive"
    );
}

#[test]
fn replay_export_refuses_checkpoint_for_other_agent() {
    let f = fixture("replay.agent", "rev_sha256:replay-001");
    // Save a checkpoint whose agent_id is somebody else's.
    let ckpt = SessionCheckpoint {
        egress_labels: Default::default(),
        history: vec![Message::system("test")],
        turn_counter: 1,
        session_state: Default::default(),
        tool_tier_escalated: false,
        discovered_tools: Default::default(),
        blocked_state_event_emitted: false,
        loop_guard_state: LoopGuard {
            max_loops_without_progress: 10,
            max_tool_failures: 5,
            max_consecutive_same_progress: 2,
            max_child_failures: 3,
            current_loops: 0,
            tool_failure_counts: std::collections::HashMap::new(),
            last_progress_fingerprint: None,
            consecutive_progress_count: 0,
            child_failure_count: 0,
            ..Default::default()
        },
        agent_id: "different.agent".to_string(),
        session_id: "x-session".to_string(),
        turn_id: "t01".to_string(),
        workflow_id: None,
        task_id: None,
        runtime_lock_hash: None,
        constitution_version: None,
        constitution_digest: None,
        llm_config_snapshot: None,
        tool_registry_version: None,
        yield_reason: YieldReason::Hibernation,
        content_store_refs: vec![],
        created_at: "2026-05-28T00:00:00Z".to_string(),
        pending_tool_state: None,
        llm_rounds_consumed: 1,
        tool_invocations_consumed: 0,
        tokens_consumed: 0,
        estimated_cost_usd: 0.0,
        compression_metadata: None,
        capsule_state: None,
        assistant_message: None,
        pending_action: None,
        suspended_at: None,
        suppress_until_turn: 0,
        trajectory_last_level: None,
            feedback_events: vec![],
    };
    save_checkpoint(&f.config, &ckpt).unwrap();

    let out_dir = tempdir().unwrap();
    let archive = out_dir.path().join("rep.capsule.tar.zst");
    let err = export(
        ExportRequest {
            agent_id: "replay.agent".to_string(),
            revision_id: None,
            mode: CapsuleMode::Replay,
            include_memory: Some(false),
            sign: Some(false),
            output_path: Some(archive),
            session_id: Some("x-session".to_string()),
            root_session_id: None,
            destination_sink: None,
            trust_domain: None,
        },
        ExportContext {
            gateway_dir: &f.gateway_dir,
            gateway_config: &f.config,
            gateway_store: &f.store,
        },
    )
    .expect_err("export must refuse a checkpoint owned by a different agent");
    assert!(
        err.to_string().contains("different.agent"),
        "unexpected error: {err}"
    );
}

#[test]
fn import_refuses_traversal_in_memory_content_handle() {
    let f = fixture("trav.agent", "rev_sha256:trav-001");
    let out_dir = tempdir().unwrap();
    let archive = out_dir.path().join("trav.capsule.tar.zst");
    export(
        ExportRequest {
            agent_id: "trav.agent".to_string(),
            revision_id: None,
            mode: CapsuleMode::Thin,
            include_memory: Some(true),
            sign: Some(false),
            output_path: Some(archive.clone()),
            session_id: None,
            root_session_id: None,
            destination_sink: None,
            trust_domain: None,
        },
        ExportContext {
            gateway_dir: &f.gateway_dir,
            gateway_config: &f.config,
            gateway_store: &f.store,
        },
    )
    .expect("export");

    // Tamper: rewrite the manifest's memory_snapshot.content_handle to
    // an absolute path.
    let work = tempdir().unwrap();
    autonoetic_gateway::capsule::archive::unpack(&archive, work.path(), 16 * 1024 * 1024).unwrap();
    let manifest_path = work.path().join("capsule.json");
    let mut m: autonoetic_types::capsule::CapsuleManifest =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    if let Some(ref mut snap) = m.memory_snapshot {
        snap.content_handle = "/etc/passwd".to_string();
    }
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&m).unwrap()).unwrap();
    autonoetic_gateway::capsule::archive::pack(work.path(), &archive).unwrap();

    let r = fresh_fixture();
    let err = import(
        ImportRequest {
            archive_path: archive,
            verify_signature: false,
            dry_run: false,
            activate: false,
            trust_domain_override: None,
            memory_conflict_policy: MemoryConflictPolicy::KeepLocal,
        },
        ImportContext {
            gateway_dir: &r.gateway_dir,
            gateway_config: &r.config,
            gateway_store: &r.store,
        },
    )
    .expect_err("absolute content_handle must be refused");
    assert!(
        err.to_string().contains("memory content_handle")
            || err.to_string().contains("absolute"),
        "unexpected error: {err}"
    );
}
