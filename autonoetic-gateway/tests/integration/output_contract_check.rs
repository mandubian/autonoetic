//! RFC #776 Part B.1 — integration tests for the output-contract check.
//!
//! Verifies the production wiring that turns a scheduler-side
//! `output_contract_check.unmet` metadata stamp into:
//!
//! 1. a `workflow.child.resolved` workflow event with payload
//!    `failure_class: "output_contract_unmet"`,
//! 2. a pending parent-session notification carrying the same
//!    `ChildStateNotification`, and
//! 3. a task row that stays `Succeeded` (the contract failure does not alter
//!    status — only the published notification's failure_class).
//!
//! Drives `workflow_store::update_task_run_status` directly against a real
//! `GatewayStore` (the same chokepoint the scheduler calls after child
//! completion). The unit tests in `scheduler/workflow_store.rs` cover the
//! pure functions (`check_output_contract`, `record_output_contract_check`,
//! `build_child_state_notification`); these tests cover the round-trip
//! through SQLite + the workflow-event/parent-notification emission path —
//! the integration gap the unit tests cannot reach.
//!
//! Does NOT exercise the `spawn_task_execution` path (which would require a
//! stub LLM + child turn execution); the four-line mapping inside
//! `spawn_task_execution` from `spawn_result.files`/`artifacts` to the
//! `produced_content_names`/`produced_artifact_files` args is already covered
//! by `check_output_contract_finds_missing` at the unit level.

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::workflow_store::{
    ensure_workflow_for_root_session, save_task_run, update_task_run_status,
};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::notification::NotificationStatus;
use autonoetic_types::workflow::{TaskRun, TaskRunStatus};
use std::sync::Arc;
use tempfile::tempdir;

fn setup() -> anyhow::Result<(tempfile::TempDir, GatewayConfig, Arc<GatewayStore>)> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir)?;
    let config = GatewayConfig {
        runtime_dir: agents_dir.join(".gateway"),
        agents_dir,
        ..GatewayConfig::default()
    };
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    Ok((temp, config, store))
}

fn seed_running_task(
    config: &GatewayConfig,
    store: &GatewayStore,
    root_session_id: &str,
    task_id: &str,
    metadata: Option<serde_json::Value>,
) -> anyhow::Result<String> {
    let run = ensure_workflow_for_root_session(
        config,
        Some(store),
        root_session_id,
        Some("planner.default"),
    )?;
    let workflow_id = run.workflow_id;

    let task = TaskRun {
        task_id: task_id.to_string(),
        workflow_id: workflow_id.clone(),
        agent_id: "coder.default".to_string(),
        session_id: format!("{root_session_id}/{task_id}"),
        parent_session_id: root_session_id.to_string(),
        status: TaskRunStatus::Running,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        source_agent_id: Some("planner.default".to_string()),
        result_summary: None,
        join_group: None,
        message: Some("build the thing".to_string()),
        metadata,
        retry_count: 0,
        last_failure_class: None,
        retry_policy: None,
        side_effect_state: None,
        dedupe_key: None,
    };
    save_task_run(config, Some(store), &task)?;
    Ok(workflow_id)
}

/// Positive case: child succeeded but a declared output is missing.
/// The B.1 metadata stamp must surface as `failure_class:
/// OutputContractUnmet` on BOTH the published notification AND the
/// `workflow.child.resolved` workflow event — without altering the
/// task's `Succeeded` status.
#[test]
fn output_contract_unmet_surfaces_on_notification_and_workflow_event() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let root_session_id = "root-b1-positive";

    // Pre-stamp the task metadata exactly as `record_output_contract_check`
    // would. Two expected outputs declared; one produced; "main.py" missing.
    let metadata = serde_json::json!({
        "expected_outputs": ["SKILL.md", "main.py"],
        "output_contract_check": {
            "unmet": ["main.py"],
            "checked_at": "2026-07-13T12:00:00Z",
        },
    });

    let workflow_id = seed_running_task(
        &config,
        &store,
        root_session_id,
        "task-unmet",
        Some(metadata.clone()),
    )?;

    update_task_run_status(
        &config,
        Some(&store),
        &workflow_id,
        "task-unmet",
        TaskRunStatus::Succeeded,
        Some("done".to_string()),
        None,
        None,
    )?;

    // 1) Task row: status=Succeeded + metadata intact (B.1 stamps the
    //    notification, NOT the task status — `Succeeded` is the literal
    //    child outcome, the unmet contract is parent-facing evidence).
    let reloaded = autonoetic_gateway::scheduler::workflow_store::load_task_run(
        &config,
        Some(&store),
        &workflow_id,
        "task-unmet",
    )?
    .expect("task exists");
    assert_eq!(reloaded.status, TaskRunStatus::Succeeded);
    assert_eq!(
        reloaded.metadata.as_ref().and_then(|m| m.get("output_contract_check")),
        Some(&serde_json::json!({"unmet": ["main.py"], "checked_at": "2026-07-13T12:00:00Z"})),
        "metadata stamp survives the round-trip"
    );

    // 2) Workflow event: `workflow.child.resolved` with `failure_class:
    //    "output_contract_unmet"` in the payload (FailureClass is
    //    #[serde(rename_all = "snake_case")] — stable identifier).
    let events = store.list_workflow_events(&workflow_id)?;
    let resolved = events
        .iter()
        .find(|e| e.event_type == "workflow.child.resolved")
        .expect("workflow.child.resolved event emitted");
    assert_eq!(
        resolved.payload["failure_class"].as_str(),
        Some("output_contract_unmet"),
        "payload stamps failure_class from the metadata"
    );
    assert_eq!(
        resolved.payload["child_status"].as_str(),
        Some("succeeded"),
        "child_status reports the actual task outcome, not the failure class"
    );
    assert_eq!(
        resolved.payload["task_id"].as_str(),
        Some("task-unmet"),
    );

    // 3) Parent notification: pending row addressed to root_session_id,
    //    carrying the same ChildStateNotification payload.
    let notifications = store
        .list_notifications_for_session(root_session_id, NotificationStatus::Pending)?;
    assert_eq!(notifications.len(), 1, "exactly one pending notification");
    let n = &notifications[0];

    // The notification delivered to the parent carries the failure_class
    // stamp. Two envelope shapes are possible:
    //   - Signal::ChildStateNotification { notification: ChildStateNotification, ... }
    //     (per-child signal, fires when other tasks are still pending)
    //   - Signal::WorkflowJoinSatisfied { child_summaries: [ChildStateNotification, ...], ... }
    //     (join-satisfied signal, fires when this was the last task — the
    //     per-child signal is suppressed as redundant in that case)
    // Both carry the same ChildStateNotification content; find it.
    let inner = if n.payload.get("type").and_then(|v| v.as_str()) == Some("WorkflowJoinSatisfied") {
        n.payload
            .get("child_summaries")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .expect("WorkflowJoinSatisfied carries child_summaries[0]")
    } else {
        &n.payload["notification"]
    };
    assert_eq!(
        inner.get("failure_class").and_then(|v| v.as_str()),
        Some("output_contract_unmet"),
        "parent notification carries the failure_class stamp (envelope: {:?})",
        n.payload.get("type").and_then(|v| v.as_str()),
    );
    assert_eq!(
        inner.get("child_status").and_then(|v| v.as_str()),
        Some("succeeded"),
    );
    Ok(())
}

/// Negative case: child succeeded with all declared outputs covered (or
/// with no `expected_outputs` declared at all). No `failure_class` stamp
/// should appear on either the workflow event or the notification.
#[test]
fn output_contract_clean_does_not_stamp_failure_class() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let root_session_id = "root-b1-clean";

    // `expected_outputs` declared AND the check came back clean (unmet = []).
    let metadata = serde_json::json!({
        "expected_outputs": ["SKILL.md"],
        "output_contract_check": {
            "unmet": [],
            "checked_at": "2026-07-13T12:00:00Z",
        },
    });

    let workflow_id = seed_running_task(
        &config,
        &store,
        root_session_id,
        "task-clean",
        Some(metadata),
    )?;

    update_task_run_status(
        &config,
        Some(&store),
        &workflow_id,
        "task-clean",
        TaskRunStatus::Succeeded,
        Some("done".to_string()),
        None,
        None,
    )?;

    let events = store.list_workflow_events(&workflow_id)?;
    let resolved = events
        .iter()
        .find(|e| e.event_type == "workflow.child.resolved")
        .expect("workflow.child.resolved event emitted even on clean completion");

    // `failure_class` should be absent (null) — the metadata.unmet array
    // is empty, so build_child_state_notification does not stamp.
    let failure_class = resolved.payload.get("failure_class");
    assert!(
        failure_class.map_or(true, |v| v.is_null()),
        "no failure_class on clean completion, got: {:?}",
        failure_class
    );

    let notifications = store
        .list_notifications_for_session(root_session_id, NotificationStatus::Pending)?;
    assert_eq!(notifications.len(), 1, "clean notification still delivered");
    // Same two-envelope story as the positive test — find the
    // ChildStateNotification content regardless of wrapping.
    let inner = if notifications[0]
        .payload
        .get("type")
        .and_then(|v| v.as_str())
        == Some("WorkflowJoinSatisfied")
    {
        &notifications[0].payload["child_summaries"][0]
    } else {
        &notifications[0].payload["notification"]
    };
    // Strict match of the event-payload assertion above: clean completion
    // means `failure_class` is absent (the struct field is
    // `skip_serializing_if = "Option::is_none"`). A loose `as_str()` check
    // would silently pass if a future change emitted an unexpected
    // object/number value here — pin the actual JSON shape instead.
    let inner_fc = inner.get("failure_class");
    assert!(
        inner_fc.map_or(true, |v| v.is_null()),
        "no failure_class on clean notification, got: {:?}",
        inner_fc
    );
    Ok(())
}

/// Edge case: the B.1 stamp must only fire when status == Succeeded.
/// A failed/cancelled task that happens to carry an output_contract_check
/// metadata stamp (e.g. the child failed mid-completion after the check
/// ran) must NOT double-stamp — the failure path has its own
/// failure_class attribution.
#[test]
fn output_contract_stamp_suppressed_on_non_succeeded_status() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let root_session_id = "root-b1-failed";

    // The scheduler ran the check, found unmet outputs, but the child then
    // reported failure (e.g. crashed between check and completion). The
    // stamp should NOT be re-applied — `Failed` carries its own
    // failure_class via evaluate_stage_retry.
    let metadata = serde_json::json!({
        "output_contract_check": {
            "unmet": ["main.py"],
            "checked_at": "2026-07-13T12:00:00Z",
        },
    });

    let workflow_id = seed_running_task(
        &config,
        &store,
        root_session_id,
        "task-failed",
        Some(metadata),
    )?;

    update_task_run_status(
        &config,
        Some(&store),
        &workflow_id,
        "task-failed",
        TaskRunStatus::Failed,
        Some("child crashed".to_string()),
        None,
        None,
    )?;

    let events = store.list_workflow_events(&workflow_id)?;
    let resolved = events
        .iter()
        .find(|e| e.event_type == "workflow.child.resolved")
        .expect("workflow.child.resolved emitted on Failed too");

    // The OutputContractUnmet stamp is gated on Succeeded; the Failed
    // transition carries its own (possibly None) failure_class. The B.1
    // metadata must NOT leak through here.
    assert_ne!(
        resolved.payload.get("failure_class").and_then(|v| v.as_str()),
        Some("output_contract_unmet"),
        "B.1 stamp suppressed on non-Succeeded transitions"
    );
    Ok(())
}
