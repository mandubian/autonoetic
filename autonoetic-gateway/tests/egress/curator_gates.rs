//! Curator `promote_to_skill` mechanical egress gate (#947).
//!
//! `promote_to_skill` graduates a lesson into an agent's SKILL.md (read by
//! every future session, including remote-model ones). The gate must refuse
//! when the evidence's egress label excludes RemoteModel — from a durable
//! memory label or, failing that, the curator's session taint.

use autonoetic_gateway::runtime::curator_journal::{persist_decision_journal_entries, DecisionJournalEntry};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::egress::EgressLabel;
use autonoetic_types::memory::{MemoryObject, MemorySourceType, MemoryVisibility};
use tempfile::tempdir;

fn promote_entry(target: &str) -> DecisionJournalEntry {
    DecisionJournalEntry {
        target: target.to_string(),
        action: "promote_to_skill".to_string(),
        reason_code: "high_confidence_pattern".to_string(),
        reason_detail: Some("recurring across 3 sessions".to_string()),
        metric_values: None,
        confidence: Some(0.9),
        target_agent: Some("planner.default".to_string()),
        proposed_instruction: Some("Never call sandbox_exec directly.".to_string()),
    }
}

fn seed_evidence_memory(store: &GatewayStore, id: &str, label: Option<EgressLabel>) {
    let mut memory = MemoryObject::new(
        id.to_string(),
        "curator.evidence".to_string(),
        "memory-curator.default".to_string(),
        "memory-curator.default".to_string(),
        "session:curator-gate:evidence".to_string(),
        "evidence content".to_string(),
    );
    memory.source_type = MemorySourceType::AgentWrite;
    memory.tags = vec!["type:pattern".to_string()];
    memory.visibility = MemoryVisibility::Global;
    memory.egress_label = label;
    store.memory_upsert(&memory).unwrap();
}

#[test]
fn promote_to_skill_refused_when_evidence_label_excludes_remote_model() {
    let tmp = tempdir().unwrap();
    let store = GatewayStore::open(tmp.path()).unwrap();
    seed_evidence_memory(&store, "mem-local", Some(EgressLabel::local_only()));

    let err = persist_decision_journal_entries(
        &store,
        "curator",
        "memory-curator.default",
        "sess-cur-1",
        None,
        &[promote_entry("mem-local")],
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("promote_to_skill refused"), "unexpected error: {msg}");
    assert!(msg.contains("declassify"), "unexpected error: {msg}");
    assert!(
        store
            .memory_get_unrestricted("grad-planner.default-mem-local")
            .unwrap()
            .is_none(),
        "refused graduation must not create a skill memory"
    );
}

#[test]
fn promote_to_skill_refused_when_session_taint_excludes_remote_model() {
    // Evidence memory has no durable label → gate falls back to the
    // curator's session taint, which excludes RemoteModel → still refused.
    let tmp = tempdir().unwrap();
    let store = GatewayStore::open(tmp.path()).unwrap();
    seed_evidence_memory(&store, "mem-no-label", None);
    store
        .set_session_egress_taint("sess-cur-2", &EgressLabel::local_only())
        .unwrap();

    let err = persist_decision_journal_entries(
        &store,
        "curator",
        "memory-curator.default",
        "sess-cur-2",
        None,
        &[promote_entry("mem-no-label")],
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("promote_to_skill refused"), "unexpected error: {msg}");
}

#[test]
fn promote_to_skill_proceeds_when_evidence_allows_remote_model() {
    let tmp = tempdir().unwrap();
    let store = GatewayStore::open(tmp.path()).unwrap();
    seed_evidence_memory(&store, "mem-ok", Some(EgressLabel::unrestricted()));

    persist_decision_journal_entries(
        &store,
        "curator",
        "memory-curator.default",
        "sess-cur-3",
        None,
        &[promote_entry("mem-ok")],
    )
    .unwrap();

    // Graduation proceeds and the resulting skill memory carries the
    // evidence label.
    let grad = store
        .memory_get_unrestricted("grad-planner.default-mem-ok")
        .unwrap()
        .expect("graduation memory must exist");
    assert_eq!(grad.egress_label, Some(EgressLabel::unrestricted()));
    assert!(
        grad.content.contains("Never call sandbox_exec directly."),
        "graduation memory must carry the proposed instruction"
    );
}
