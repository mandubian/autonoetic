//! Memory curator decision journal (issue #30).
//!
//! Tests:
//! - `extract_decision_journal_entries` parses well-formed inputs and
//!   silently drops malformed ones.
//! - `persist_decision_journal_entries` emits one `curator.decision` causal
//!   event per entry plus a `decision_journal_recorded` summary event.
//! - `list_curator_decisions_by_target` round-trips a target lookup.
//! - The curator SKILL.md frontmatter parses cleanly (smoke test against
//!   the schema declaration we just added).

use std::sync::Arc;

use autonoetic_gateway::runtime::curator_journal::{
    extract_and_persist, extract_decision_journal_entries, persist_decision_journal_entries,
    DecisionJournalEntry,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;

fn temp_store() -> (tempfile::TempDir, Arc<GatewayStore>) {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());
    (temp, store)
}

#[test]
fn extract_returns_none_when_no_journal_field() {
    let reply = serde_json::json!({
        "agent_scores": {},
        "systemic_gaps": [],
        "learnings_stored": 0
    })
    .to_string();
    assert!(extract_decision_journal_entries(&reply).is_none());
}

#[test]
fn extract_returns_none_for_non_json_reply() {
    assert!(extract_decision_journal_entries("not JSON at all").is_none());
}

#[test]
fn extract_returns_empty_when_journal_is_empty_array() {
    let reply = serde_json::json!({ "decision_journal": [] }).to_string();
    let entries = extract_decision_journal_entries(&reply).unwrap();
    assert!(entries.is_empty());
}

#[test]
fn extract_parses_well_formed_entries() {
    let reply = serde_json::json!({
        "decision_journal": [
            {
                "target": "memory://sess-1/turn-3",
                "action": "drop",
                "reason_code": "low_signal",
                "reason_detail": "too few corroborating sessions",
                "metric_values": { "supporting_sessions": 1 },
                "confidence": 0.7
            },
            {
                "target": "agent.coder.default",
                "action": "flag_for_evolution",
                "reason_code": "eval_regression"
            }
        ]
    })
    .to_string();
    let entries = extract_decision_journal_entries(&reply).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].target, "memory://sess-1/turn-3");
    assert_eq!(entries[0].action, "drop");
    assert_eq!(entries[0].reason_code, "low_signal");
    assert_eq!(entries[0].confidence, Some(0.7));
    assert_eq!(
        entries[0].metric_values,
        Some(serde_json::json!({ "supporting_sessions": 1 }))
    );
    assert_eq!(entries[1].target, "agent.coder.default");
    assert!(entries[1].reason_detail.is_none());
}

#[test]
fn extract_drops_malformed_entries() {
    let reply = serde_json::json!({
        "decision_journal": [
            // missing target
            { "action": "drop", "reason_code": "low_signal" },
            // empty target
            { "target": "", "action": "drop", "reason_code": "low_signal" },
            // valid
            { "target": "memory://x/y", "action": "keep", "reason_code": "high_confidence_pattern" },
            // missing reason_code
            { "target": "memory://a/b", "action": "drop" },
        ]
    })
    .to_string();
    let entries = extract_decision_journal_entries(&reply).unwrap();
    assert_eq!(entries.len(), 1, "only one well-formed entry");
    assert_eq!(entries[0].target, "memory://x/y");
    assert_eq!(entries[0].reason_code, "high_confidence_pattern");
}

#[test]
fn persist_emits_one_event_per_entry_plus_summary() {
    let (_temp, store) = temp_store();
    let entries = vec![
        DecisionJournalEntry {
            target_agent: None,
            proposed_instruction: None,
            target: "memory://s/a".to_string(),
            action: "drop".to_string(),
            reason_code: "low_signal".to_string(),
            reason_detail: Some("only 1 session".to_string()),
            metric_values: None,
            confidence: Some(0.5),
        },
        DecisionJournalEntry {
            target_agent: None,
            proposed_instruction: None,
            target: "memory://s/b".to_string(),
            action: "keep".to_string(),
            reason_code: "high_confidence_pattern".to_string(),
            reason_detail: None,
            metric_values: Some(serde_json::json!({ "uses": 12 })),
            confidence: None,
        },
    ];
    persist_decision_journal_entries(
        store.as_ref(),
        "curator",
        "memory-curator.default",
        "sess-xyz",
        Some("rev-1"),
        &entries,
    )
    .unwrap();

    let events = store
        .search_causal_events(Some("sess-xyz"), None, 100)
        .unwrap();
    // 2 per-entry events + 1 summary
    assert_eq!(events.len(), 3);
    let per_entry: Vec<_> = events
        .iter()
        .filter(|e| e.action == "decision")
        .collect();
    assert_eq!(per_entry.len(), 2);
    let summary: Vec<_> = events
        .iter()
        .filter(|e| e.action == "decision_journal_recorded")
        .collect();
    assert_eq!(summary.len(), 1);
    let summary_payload: serde_json::Value =
        serde_json::from_str(summary[0].payload.as_deref().unwrap()).unwrap();
    assert_eq!(summary_payload["entry_count"], 2);
    assert_eq!(summary_payload["revision_id"], "rev-1");
}

#[test]
fn query_by_target_returns_only_that_target() {
    let (_temp, store) = temp_store();
    let entries = vec![
        DecisionJournalEntry {
            target_agent: None,
            proposed_instruction: None,
            target: "memory://hot/x".to_string(),
            action: "drop".to_string(),
            reason_code: "low_signal".to_string(),
            reason_detail: None,
            metric_values: None,
            confidence: None,
        },
        DecisionJournalEntry {
            target_agent: None,
            proposed_instruction: None,
            target: "memory://cold/y".to_string(),
            action: "keep".to_string(),
            reason_code: "high_confidence_pattern".to_string(),
            reason_detail: None,
            metric_values: None,
            confidence: None,
        },
        DecisionJournalEntry {
            target_agent: None,
            proposed_instruction: None,
            target: "memory://hot/x".to_string(),
            action: "flag_for_evolution".to_string(),
            reason_code: "eval_regression".to_string(),
            reason_detail: Some("found again next run".to_string()),
            metric_values: None,
            confidence: None,
        },
    ];
    persist_decision_journal_entries(
        store.as_ref(),
        "curator",
        "memory-curator.default",
        "sess-1",
        None,
        &entries,
    )
    .unwrap();

    let hits = store
        .list_curator_decisions_by_target("memory://hot/x", 50)
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits
        .iter()
        .all(|e| e.target.as_deref() == Some("memory://hot/x")));
    assert!(hits
        .iter()
        .all(|e| e.category == "curator" && e.action == "decision"));
}

#[test]
fn extract_and_persist_skips_when_no_journal_field() {
    let (_temp, store) = temp_store();
    let n = extract_and_persist(
        store.as_ref(),
        "curator",
        "memory-curator.default",
        "sess-no-journal",
        None,
        r#"{"agent_scores":{},"systemic_gaps":[]}"#,
    )
    .unwrap();
    assert_eq!(n, 0);
    let events = store
        .search_causal_events(Some("sess-no-journal"), None, 10)
        .unwrap();
    assert!(events.is_empty());
}

#[test]
fn extract_and_persist_persists_well_formed_entries() {
    let (_temp, store) = temp_store();
    let reply = serde_json::json!({
        "decision_journal": [
            { "target": "memory://t/1", "action": "drop", "reason_code": "stale" }
        ]
    })
    .to_string();
    let n = extract_and_persist(
        store.as_ref(),
        "curator",
        "memory-curator.default",
        "sess-go",
        Some("rev-9"),
        &reply,
    )
    .unwrap();
    assert_eq!(n, 1);
    let hits = store
        .list_curator_decisions_by_target("memory://t/1", 10)
        .unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn curator_skill_md_parses_with_new_schema() {
    // The frontmatter now declares io.returns. Smoke-test that the existing
    // agent-parser machinery still loads the manifest end-to-end.
    let skill_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("agents/evolution/memory-curator.default/SKILL.md");
    let text = std::fs::read_to_string(&skill_path).expect("read curator SKILL.md");
    let (manifest, _body) = autonoetic_gateway::runtime::parser::SkillParser::parse(&text)
        .expect("curator SKILL.md must parse with the issue #30 schema additions");
    let io = manifest
        .io
        .as_ref()
        .expect("curator manifest must carry io block");
    let returns = io
        .returns
        .as_ref()
        .expect("curator manifest must declare io.returns");
    let required = returns
        .get("required")
        .and_then(|v: &serde_json::Value| v.as_array())
        .expect("io.returns.required must be an array");
    let required_keys: Vec<&str> = required
        .iter()
        .filter_map(|v: &serde_json::Value| v.as_str())
        .collect();
    assert!(required_keys.contains(&"decision_journal"));
    assert!(required_keys.contains(&"agent_scores"));
    assert!(required_keys.contains(&"systemic_gaps"));
}
