//! Integration tests for the operator activity feed (#353–#355).

use autonoetic_gateway::runtime::operator_activity::classify_tool_activity;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::operator_activity::OperatorActivitySeverity;
use tempfile::tempdir;

#[test]
fn content_write_persists_and_lists_by_root_session() {
    let dir = tempdir().unwrap();
    let store = GatewayStore::open(dir.path()).unwrap();
    let root = "session-op-activity";

    let draft = classify_tool_activity(
        "content_write",
        r#"{"name":"news_fetcher.py"}"#,
        r#"{"ok":true,"name":"news_fetcher.py"}"#,
    )
    .expect("content_write should classify");

    let record = draft.into_record(
        root.to_string(),
        root.to_string(),
        "planner.collaborative".to_string(),
        None,
        None,
        Some("turn-1".to_string()),
        Some("content_write".to_string()),
        Some("evt-content-write".to_string()),
        None,
    );
    store.insert_operator_activity(&record).unwrap();

    let listed = store
        .list_operator_activity(root, None, 10, Some(OperatorActivitySeverity::Progress))
        .unwrap();
    assert_eq!(listed.activities.len(), 1);
    assert!(listed.activities[0].summary.contains("news_fetcher.py"));
}

#[test]
fn execution_search_success_not_persisted_by_classifier() {
    assert!(classify_tool_activity(
        "execution_search",
        r#"{"command_pattern":"%"}"#,
        r#"{"ok":true,"count":3}"#,
    )
    .is_none());
}

#[test]
fn causal_event_dedup_on_insert() {
    let dir = tempdir().unwrap();
    let store = GatewayStore::open(dir.path()).unwrap();
    let root = "session-dedup";

    let draft = classify_tool_activity(
        "content_write",
        r#"{"name":"a.py"}"#,
        r#"{"ok":true,"name":"a.py"}"#,
    )
    .unwrap();

    let first = draft.into_record(
        root.to_string(),
        root.to_string(),
        "planner.default".to_string(),
        None,
        None,
        None,
        Some("content_write".to_string()),
        Some("evt-dedup-1".to_string()),
        None,
    );
    store.insert_operator_activity(&first).unwrap();

    let mut duplicate = first.clone();
    duplicate.summary = "duplicate".to_string();
    store.insert_operator_activity(&duplicate).unwrap();

    let listed = store.list_operator_activity(root, None, 10, None).unwrap();
    assert_eq!(listed.activities.len(), 1);
    assert!(listed.activities[0].summary.contains("a.py"));
}
