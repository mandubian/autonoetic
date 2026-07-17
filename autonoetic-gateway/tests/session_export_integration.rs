//! Integration tests for `autonoetic_gateway::runtime::session_export`.

mod support;

use autonoetic_gateway::runtime::session_export::{
    export_session, render_export, ExportFormat, ExportOptions,
};
use autonoetic_gateway::scheduler::gateway_store::{
    GatewayStore, LiveDigestEventRecord,
};
use autonoetic_types::principal::Principal;
use autonoetic_types::session_outcome::{Completion, OperatorThumb};
use autonoetic_types::session_timeline::{Altitude, SessionRole};
use support::TestWorkspace;

fn seed_outcome(store: &GatewayStore, root: &str) {
    store
        .upsert_session_outcome_metrics(
            root,
            root,
            "planner.default",
            Some("test the export utility"),
            3,
            1234,
            0.001,
            5.0,
        )
        .unwrap();
    store
        .set_session_outcome_grade(root, "grader.test", Completion::Achieved, None)
        .unwrap();
    store
        .set_session_outcome_operator_rating(root, OperatorThumb::Up, Some("worked well"))
        .unwrap();
}

fn seed_timeline_event(store: &GatewayStore, root: &str, event_type: &str, altitude: Altitude) {
    let principal = Principal::agent("planner.default".to_string());
    store
        .create_live_digest_event(
            &LiveDigestEventRecord {
                event_id: format!("{}-evt-{}", root, event_type),
                root_session_id: root.to_string(),
                source_session_id: root.to_string(),
                turn_id: Some("turn-000001".to_string()),
                source_agent_id: Some("planner.default".to_string()),
                source_node_id: "gateway".to_string(),
                event_type: event_type.to_string(),
                payload: Some("user asked for export test".to_string()),
                created_at: chrono::Utc::now().to_rfc3339(),
                principal_kind: Some(principal.kind_to_storage()),
                principal_id: Some(principal.id.clone()),
                role: Some(SessionRole::Planner.to_storage()),
                altitude: Some(altitude.as_str().to_string()),
                refs_json: None,
            },
        )
        .unwrap();
}

#[test]
fn test_export_session_includes_metadata_timeline_and_outcome() {
    let workspace = TestWorkspace::new().unwrap();
    let gateway_dir = workspace.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = GatewayStore::open(&gateway_dir).unwrap();
    let config = workspace.gateway_config();

    let root = "root-export-test";
    seed_outcome(&store, root);
    seed_timeline_event(&store, root, "session.start", Altitude::Normal);

    let opts = ExportOptions {
        format: ExportFormat::Room,
        ..ExportOptions::default()
    };

    let export = export_session(&store, &config, root, &opts
    )
    .expect("export_session should succeed");

    assert_eq!(export.session_id, root);
    assert_eq!(export.root_session_id, root);
    assert!(export.outcome.is_some());
    assert_eq!(export.timeline.entries.len(), 1);

    let md = render_export(&export, &opts).expect("render should succeed");
    assert!(md.contains(&format!("# Session Export: `{}`", root)));
    assert!(md.contains("planner.default"));
    assert!(md.contains("test the export utility"));
    assert!(md.contains("worked well"));
    assert!(md.contains("session.start"));
}

#[test]
fn test_export_session_json_format_roundtrips() {
    let workspace = TestWorkspace::new().unwrap();
    let gateway_dir = workspace.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = GatewayStore::open(&gateway_dir).unwrap();
    let config = workspace.gateway_config();

    let root = "root-export-json";
    store
        .upsert_session_outcome_metrics(root, root, "coder.default", None, 1, 0, 0.0, 1.0)
        .unwrap();
    store
        .set_session_outcome_grade(root, "grader.test", Completion::Failed, None)
        .unwrap();

    let opts = ExportOptions {
        format: ExportFormat::Json,
        ..ExportOptions::default()
    };

    let export = export_session(&store, &config, root, &opts
    )
    .unwrap();
    let json_text = render_export(&export, &opts).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_text).unwrap();
    assert_eq!(parsed["session_id"], root);
    assert_eq!(parsed["root_session_id"], root);
}

#[test]
fn test_export_session_min_altitude_filters_detail_events() {
    let workspace = TestWorkspace::new().unwrap();
    let gateway_dir = workspace.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = GatewayStore::open(&gateway_dir).unwrap();
    let config = workspace.gateway_config();

    let root = "root-altitude-filter";
    seed_timeline_event(&store, root, "turn.start", Altitude::Detail);
    seed_timeline_event(&store, root, "llm.request_failed", Altitude::Error);

    let opts = ExportOptions {
        format: ExportFormat::Room,
        min_altitude: Some(Altitude::Attention),
        ..ExportOptions::default()
    };

    let export = export_session(&store, &config, root, &opts
    )
    .unwrap();

    assert_eq!(export.timeline.entries.len(), 1);
    assert_eq!(export.timeline.entries[0].event_type, "llm.request_failed");
}
