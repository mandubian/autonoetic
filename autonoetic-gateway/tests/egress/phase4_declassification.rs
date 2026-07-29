//! Phase 4 (#909) slice 1: operator declassification grants + curator exception.

use autonoetic_gateway::runtime::curator_journal::{
    persist_decision_journal_entries, DecisionJournalEntry,
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
