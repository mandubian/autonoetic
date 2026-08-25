//! Phase 4 (#909) slice 1: operator declassification grants + curator exception.

use autonoetic_gateway::runtime::curator_journal::{
    persist_decision_journal_entries, DecisionJournalEntry,
};
use autonoetic_gateway::runtime::egress_labeler::{
    session_host_network_declass_target, snapshot_session_egress_taint_for_approval,
};
use autonoetic_gateway::runtime::active_execution_registry::{
    ActiveExecutionRegistry, NativeToolRunContext,
};
use autonoetic_gateway::scheduler::approval::{apply_decision, ApproveOptions, DecisionContext};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{
    ApprovalDecision, ApprovalLevel, ApprovalRequest, ApprovalStatus, GrantScope,
    ScheduledAction,
};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::egress::{EgressDeclassificationTarget, EgressLabel, Sink};
use autonoetic_types::memory::{MemoryObject, MemorySourceType, MemoryVisibility};

fn local_only_memory(id: &str) -> MemoryObject {
    let mut memory = MemoryObject::new(
        id.to_string(),
        "evolution/evidence".to_string(),
        "memory-curator.default".to_string(),
        "memory-curator.default".to_string(),
        "session:root-909/curator".to_string(),
        "secret pattern".to_string(),
    );
    memory.egress_label = Some(EgressLabel::local_only());
    memory.source_type = MemorySourceType::AgentWrite;
    memory.visibility = MemoryVisibility::Global;
    memory
}

#[test]
fn declassification_grant_allows_curator_promote_to_skill() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;
    let memory_id = "mem-local-909";
    store.memory_upsert(&local_only_memory(memory_id))?;

    store.insert_egress_declassification_grant(
        "root-909",
        "root-909/curator",
        "memory-curator.default",
        &EgressDeclassificationTarget::MemoryId(memory_id.to_string()),
        Sink::RemoteModel,
        &GrantScope::RootSession,
        "operator",
        &chrono::Utc::now().to_rfc3339(),
        None,
        None,
    )?;

    let entry = DecisionJournalEntry {
        target: memory_id.to_string(),
        action: "promote_to_skill".to_string(),
        reason_code: "high_confidence_pattern".to_string(),
        reason_detail: Some("recurring".to_string()),
        metric_values: None,
        confidence: Some(0.9),
        target_agent: Some("planner.default".to_string()),
        proposed_instruction: Some("Never call sandbox_exec directly.".to_string()),
    };

    persist_decision_journal_entries(
        &store,
        "curator",
        "memory-curator.default",
        "root-909/curator",
        None,
        std::slice::from_ref(&entry),
    )?;

    let grad_id = format!("grad-planner.default-{}", memory_id);
    assert!(
        store.memory_get(&grad_id)?.is_some(),
        "graduation memory should be written when declassification grant is active"
    );
    Ok(())
}

#[test]
fn promote_to_skill_still_refused_without_grant() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;
    let memory_id = "mem-local-no-grant";
    store.memory_upsert(&local_only_memory(memory_id))?;

    let entry = DecisionJournalEntry {
        target: memory_id.to_string(),
        action: "promote_to_skill".to_string(),
        reason_code: "high_confidence_pattern".to_string(),
        reason_detail: Some("recurring".to_string()),
        metric_values: None,
        confidence: Some(0.9),
        target_agent: Some("planner.default".to_string()),
        proposed_instruction: Some("Never call sandbox_exec directly.".to_string()),
    };

    let err = persist_decision_journal_entries(
        &store,
        "curator",
        "memory-curator.default",
        "root-909/curator",
        None,
        std::slice::from_ref(&entry),
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("promote_to_skill refused"),
        "expected mechanical refuse without grant, got: {err}"
    );
    Ok(())
}

#[test]
fn apply_decision_materializes_grant_and_emits_declassified() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;
    let config = GatewayConfig::default();
    let target = EgressDeclassificationTarget::MemoryId("mem-approve-909".to_string());

    let decision = ApprovalDecision {
        request_id: "apr-declass-909".to_string(),
        session_id: "root-909/curator".to_string(),
        root_session_id: Some("root-909".to_string()),
        agent_id: "memory-curator.default".to_string(),
        action: ScheduledAction::EgressDeclassify {
            target: target.clone(),
            allowed_sink: Sink::RemoteModel,
            reason: "operator widens for graduation".to_string(),
            payload: None,
        },
        status: ApprovalStatus::Approved,
        decided_by: "operator".to_string(),
        decided_at: chrono::Utc::now().to_rfc3339(),
        reason: Some("graduate local evidence".to_string()),
        workflow_id: None,
        task_id: None,
        approval_level: ApprovalLevel::Operator,
    };

    apply_decision(
        &config,
        Some(&store),
        &decision,
        &ApproveOptions::default(),
        &DecisionContext {
            wiki_materialized_meta: None,
            hook_executor: None,
        },
    )?;

    assert!(store.egress_declassification_allows(
        &target,
        Sink::RemoteModel,
        "root-909/curator",
        "root-909",
    )?);

    let events = store.search_causal_events(Some("root-909/curator"), None, 50)?;
    assert!(
        events.iter().any(|e| e.action == "egress.declassified"),
        "approval should emit egress.declassified"
    );
    Ok(())
}


#[test]
fn approval_snapshot_persists_in_memory_taint_before_finalize() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;
    let session_id = "root-snap/researcher";
    let root = "root-snap";
    let ctx = NativeToolRunContext {
        registry: ActiveExecutionRegistry::new(),
        root_session_id: root.to_string(),
        workflow_id: None,
        task_id: None,
        session_id: session_id.to_string(),
        agent_id: "researcher.default".into(),
        live_digest: None,
        live_report: None,
        user_id: None,
        artifact_id: None,
        sentinel_suppress_target: None,
        discovered_tools: None,
            annotation_counter: None,
        tool_discovery_catalog: None,
        wake_hint: None,
        wake_hints_map: None,
        egress_taint: Some(EgressLabel::local_only()),
        egress_query_sink: None,
    };
    assert_eq!(store.get_session_egress_taint(session_id)?, None);

    snapshot_session_egress_taint_for_approval(&store, session_id, Some(&ctx))?;
    assert_eq!(
        store.get_session_egress_taint(session_id)?,
        Some(EgressLabel::local_only())
    );

    let decision = ApprovalDecision {
        request_id: "apr-snap".to_string(),
        session_id: session_id.to_string(),
        root_session_id: Some(root.to_string()),
        agent_id: "researcher.default".to_string(),
        action: ScheduledAction::WebFetch {
            url: "https://example.com/doc".to_string(),
            timeout_secs: Some(20),
            max_chars: Some(10_000),
            detected_hosts: Some(vec!["example.com".to_string()]),
            payload: None,
        },
        status: ApprovalStatus::Approved,
        decided_by: "operator".to_string(),
        decided_at: chrono::Utc::now().to_rfc3339(),
        reason: None,
        workflow_id: None,
        task_id: None,
        approval_level: ApprovalLevel::Operator,
    };
    apply_decision(
        &GatewayConfig::default(),
        Some(&store),
        &decision,
        &ApproveOptions::default(),
        &DecisionContext {
            wiki_materialized_meta: None,
            hook_executor: None,
        },
    )?;

    assert!(store.egress_declassification_allows(
        &session_host_network_declass_target(root, "example.com"),
        Sink::Network,
        session_id,
        root,
    )?);
    Ok(())
}

#[test]
fn delete_session_grants_clears_declassification_rows() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;
    store.insert_egress_declassification_grant(
        "root-clean",
        "root-clean/curator",
        "memory-curator.default",
        &EgressDeclassificationTarget::MemoryId("mem-x".to_string()),
        Sink::RemoteModel,
        &GrantScope::RootSession,
        "operator",
        &chrono::Utc::now().to_rfc3339(),
        None,
        None,
    )?;
    store.delete_session_grants("root-clean")?;
    assert!(!store.egress_declassification_allows(
        &EgressDeclassificationTarget::MemoryId("mem-x".to_string()),
        Sink::RemoteModel,
        "root-clean/curator",
        "root-clean",
    )?);
    Ok(())
}

/// `list_egress_declassification_grants` (the new operator-facing listing
/// wrapper) round-trips with the #961 host-scoped revoke: list shows active
/// grants, revoke-by-host soft-deletes the matching one, enforcement
/// fail-closes, and re-list drops it. The listing wrapper is the only piece
/// this branch adds on top of #961's revoke surface.
#[test]
fn list_egress_declassification_grants_round_trips_with_host_revoke() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;
    // Host-scoped source_pattern targets, the shape #961's approval path and
    // host revocation both key on (session:<root>:host:<host>).
    let target_a = EgressDeclassificationTarget::SourcePattern(
        "session:root-list:host:example.com".to_string(),
    );
    let target_b = EgressDeclassificationTarget::SourcePattern(
        "session:root-list:host:other.com".to_string(),
    );

    store.insert_egress_declassification_grant(
        "root-list",
        "root-list/curator",
        "memory-curator.default",
        &target_a,
        Sink::Network,
        &GrantScope::RootSession,
        "operator",
        &chrono::Utc::now().to_rfc3339(),
        None,
        None,
    )?;
    store.insert_egress_declassification_grant(
        "root-list",
        "root-list/curator",
        "memory-curator.default",
        &target_b,
        Sink::Network,
        &GrantScope::RootSession,
        "operator",
        &chrono::Utc::now().to_rfc3339(),
        None,
        None,
    )?;

    // Both active and listed; other root sessions are isolated.
    let active = store.list_egress_declassification_grants("root-list")?;
    assert_eq!(active.len(), 2);
    assert!(store
        .list_egress_declassification_grants("root-other")?
        .is_empty());

    // target_a is authorized before revoke.
    assert!(store.egress_declassification_allows(
        &target_a,
        Sink::Network,
        "root-list/curator",
        "root-list",
    )?);

    // #961's host-scoped revoke closes only the matching host grant.
    let revoked = store.revoke_egress_declassification_grants(
        "root-list",
        Some("example.com"),
        "operator live revoke",
    )?;
    assert_eq!(revoked, 1, "host-scoped revoke hits exactly one grant");

    // Enforcement fail-closes on the revoked grant; the sibling stays active.
    assert!(!store.egress_declassification_allows(
        &target_a,
        Sink::Network,
        "root-list/curator",
        "root-list",
    )?);
    assert!(store.egress_declassification_allows(
        &target_b,
        Sink::Network,
        "root-list/curator",
        "root-list",
    )?);

    // Re-list drops the revoked row (the audit row stays in the table).
    let active = store.list_egress_declassification_grants("root-list")?;
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].target, target_b);
    Ok(())
}

#[test]
fn session_sink_declassified_tracks_grant_lifecycle() -> anyhow::Result<()> {
    use autonoetic_gateway::runtime::egress_labeler::{
        session_egress_declass_target, session_sink_declassified,
    };
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;
    let root = "root-sink";
    let session = "root-sink/coder";

    // No grant → not declassified.
    assert!(!session_sink_declassified(&store, session, root, Sink::RemoteModel));

    // Session-wide grant × RemoteModel → declassified…
    store.insert_egress_declassification_grant(
        root,
        session,
        "coder.default",
        &session_egress_declass_target(root),
        Sink::RemoteModel,
        &GrantScope::RootSession,
        "operator",
        &chrono::Utc::now().to_rfc3339(),
        None,
        None,
    )?;
    assert!(session_sink_declassified(&store, session, root, Sink::RemoteModel));
    // …but only for that sink — a RemoteModel grant is not a Network grant.
    assert!(!session_sink_declassified(&store, session, root, Sink::Network));

    // Revoke (--all shape) → closed again.
    let revoked = store.revoke_egress_declassification_grants(root, None, "test")?;
    assert_eq!(revoked, 1);
    assert!(!session_sink_declassified(&store, session, root, Sink::RemoteModel));
    Ok(())
}

#[test]
fn declassify_offer_files_once_and_dedups() -> anyhow::Result<()> {
    use autonoetic_gateway::runtime::egress_labeler::file_declassify_offer;
    let tmp = tempfile::tempdir()?;
    let store = std::sync::Arc::new(GatewayStore::open(tmp.path())?);
    let config = GatewayConfig::default();
    let manifest = autonoetic_types::agent::AgentManifest {
        agent: autonoetic_types::agent::AgentIdentity {
            id: "coder.default".to_string(),
            name: "Coder".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        ..Default::default()
    };
    let root = "root-offer";
    let session = "root-offer/coder";

    let id1 = file_declassify_offer(
        &store,
        &config,
        session,
        root,
        &manifest,
        &EgressLabel::local_only(),
    )
    .expect("offer filed");
    assert!(id1.starts_with("apr-"));

    // Second refusal reuses the same pending request — no flood.
    let id2 = file_declassify_offer(
        &store,
        &config,
        session,
        root,
        &manifest,
        &EgressLabel::local_only(),
    )
    .expect("offer reused");
    assert_eq!(id1, id2);

    // A differently-labeled batch still dedups onto the same offer: the
    // payload is canonical; the label lives in the DecisionContext only.
    // Both labels exclude RemoteModel, so either could trigger the refusal.
    let id3 = file_declassify_offer(
        &store,
        &config,
        session,
        root,
        &manifest,
        &EgressLabel::no_remote_model(),
    )
    .expect("offer reused across batch labels");
    assert_eq!(id1, id3);

    let pending = store.get_pending_approvals_for_root(root)?;
    assert_eq!(pending.len(), 1, "exactly one pending offer");
    match &pending[0].action {
        ScheduledAction::EgressDeclassify {
            target,
            allowed_sink,
            ..
        } => {
            assert_eq!(target.value(), format!("session:{root}"));
            assert_eq!(*allowed_sink, Sink::RemoteModel);
        }
        other => panic!("expected EgressDeclassify, got {}", other.kind()),
    }
    Ok(())
}

/// `revoke_egress_declassification_grant_by_id` — the by-id revoke path the TUI
/// grants panel uses ("select a row → revoke it"). Covers the case the
/// host-scoped path (#961) can't reach: a `MemoryId`/`EnvelopeId` target with no
/// host. Revokes exactly one, enforcement fail-closes, is idempotent, and
/// leaves sibling grants active.
#[test]
fn revoke_egress_declassification_grant_by_id_targets_memory_grant() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;
    let root = "root-by-id";
    // A MemoryId declass grant (no host) + a sibling host-scoped grant.
    let target_a = EgressDeclassificationTarget::MemoryId("mem-by-id".to_string());
    let target_b = EgressDeclassificationTarget::SourcePattern(format!(
        "session:{root}:host:example.com"
    ));
    store.insert_egress_declassification_grant(
        root,
        root,
        "memory-curator.default",
        &target_a,
        Sink::RemoteModel,
        &GrantScope::RootSession,
        "operator",
        &chrono::Utc::now().to_rfc3339(),
        None,
        None,
    )?;
    store.insert_egress_declassification_grant(
        root,
        root,
        "memory-curator.default",
        &target_b,
        Sink::Network,
        &GrantScope::RootSession,
        "operator",
        &chrono::Utc::now().to_rfc3339(),
        None,
        None,
    )?;

    let active = store.list_egress_declassification_grants(root)?;
    assert_eq!(active.len(), 2);

    let gid_a = active
        .iter()
        .find(|g| g.target == target_a)
        .expect("MemoryId grant present")
        .id;

    // target_a authorized before revoke.
    assert!(store.egress_declassification_allows(
        &target_a,
        Sink::RemoteModel,
        root,
        root,
    )?);

    // By-id revoke: hits exactly the MemoryId grant, transitions active → revoked.
    assert!(
        store.revoke_egress_declassification_grant_by_id(root, gid_a, "operator: tui revoke")?,
        "first revoke transitions active -> revoked"
    );
    assert!(
        !store.revoke_egress_declassification_grant_by_id(root, gid_a, "operator: tui revoke")?,
        "second revoke is an idempotent no-op"
    );
    // Missing id: no-op, not an error.
    assert!(
        !store.revoke_egress_declassification_grant_by_id(root, gid_a + 99999, "x")?
    );

    // Enforcement fail-closes on the revoked grant; the sibling host-scoped
    // grant is untouched (host-scoped revoke could not have done this without
    // also being given the host).
    assert!(!store.egress_declassification_allows(
        &target_a,
        Sink::RemoteModel,
        root,
        root,
    )?);
    assert!(store.egress_declassification_allows(
        &target_b,
        Sink::Network,
        root,
        root,
    )?);

    // Re-list drops the revoked row.
    let active = store.list_egress_declassification_grants(root)?;
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].target, target_b);
    Ok(())
}

/// A row id is not a capability. Grant ids are `AUTOINCREMENT` and therefore
/// enumerable across sessions, so the by-id revoke path must be scoped to the
/// root that owns the grant: root B naming root A's id gets an idempotent
/// no-op, and A's grant keeps enforcing. Without the `root_session_id`
/// predicate any caller could revoke another session's declassification grant
/// by counting up from 1 (the isolation half of P-15.3's "revocable").
#[test]
fn revoke_egress_declassification_grant_by_id_is_scoped_to_owning_root() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;
    let root_a = "root-a-owner";
    let root_b = "root-b-stranger";
    let target = EgressDeclassificationTarget::MemoryId("mem-cross-root".to_string());
    store.insert_egress_declassification_grant(
        root_a,
        root_a,
        "memory-curator.default",
        &target,
        Sink::RemoteModel,
        &GrantScope::RootSession,
        "operator",
        &chrono::Utc::now().to_rfc3339(),
        None,
        None,
    )?;
    let gid = store
        .list_egress_declassification_grants(root_a)?
        .first()
        .expect("grant present under root A")
        .id;

    // Root B knows (or guessed) the id — the revoke must not land.
    assert!(
        !store.revoke_egress_declassification_grant_by_id(root_b, gid, "operator: wrong root")?,
        "a foreign root's by-id revoke is a no-op"
    );
    assert!(
        store.egress_declassification_allows(&target, Sink::RemoteModel, root_a, root_a)?,
        "root A's grant still enforces after root B's attempt"
    );
    assert_eq!(store.list_egress_declassification_grants(root_a)?.len(), 1);

    // The owning root can still revoke it — scoping blocks strangers, not owners.
    assert!(store.revoke_egress_declassification_grant_by_id(
        root_a,
        gid,
        "operator: tui revoke"
    )?);
    assert!(!store.egress_declassification_allows(&target, Sink::RemoteModel, root_a, root_a)?);
    Ok(())
}
