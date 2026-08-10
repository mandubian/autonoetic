//! Workflow completion guard integration tests (RFC phase 0).
//!
//! Verifies that once a workflow reaches a terminal state:
//! - `agent_spawn` from the root planner session reactivates the workflow and
//!   accepts the new task (allows follow-up work such as invoking a freshly
//!   built agent after the build workflow completed).
//! - `agent_spawn` from a child session is allowed once the root planner has
//!   reactivated the workflow (needed for install delegations such as
//!   agent-factory / specialized_builder).
//! - `workflow.force_complete` rejects mutations.
//! - Pending child-state / join-satisfied notifications are suppressed, not delivered.

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::workflow_store::{
    ensure_workflow_for_root_session, load_workflow_run, save_task_run, save_workflow_run,
    try_complete_workflow,
};
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::escalation::{EscalationMessage, EscalationStatus};
use autonoetic_types::notification::{NotificationRecord, NotificationStatus, NotificationType};
use autonoetic_types::workflow::{
    ChildStateNotification, TaskRun, TaskRunStatus, WorkflowRunStatus,
};
use std::sync::Arc;
use tempfile::tempdir;
use crate::support::manifest_builder::TestManifest;

fn planner_manifest() -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "planner.default".to_string(),
            name: "planner.default".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::AgentSpawn {
            max_children: 4,
            max_spawn_depth: 0,
        }],
        ..TestManifest::new().build()
    }
}

fn setup() -> anyhow::Result<(tempfile::TempDir, GatewayConfig, Arc<GatewayStore>)> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let parent_dir = agents_dir.join("planner.default");
    std::fs::create_dir_all(&parent_dir)?;
    let config = GatewayConfig {
        agents_dir,
        default_workflow_wait_secs: 10,
        ..GatewayConfig::default()
    };
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    Ok((temp, config, store))
}

fn seed_terminal_workflow(
    config: &GatewayConfig,
    store: &GatewayStore,
    root_session_id: &str,
    terminal_status: WorkflowRunStatus,
) -> anyhow::Result<String> {
    let run = ensure_workflow_for_root_session(
        config,
        Some(store),
        root_session_id,
        Some("planner.default"),
    )?;
    let workflow_id = run.workflow_id;

    let mut workflow = load_workflow_run(config, Some(store), &workflow_id)?
        .expect("workflow should exist after ensure");
    workflow.status = terminal_status;
    workflow.updated_at = chrono::Utc::now().to_rfc3339();
    save_workflow_run(config, Some(store), &workflow)?;

    let task = TaskRun {
        task_id: "task-old".to_string(),
        workflow_id: workflow_id.clone(),
        agent_id: "coder.default".to_string(),
        session_id: format!("{root_session_id}/coder-old"),
        parent_session_id: root_session_id.to_string(),
        status: TaskRunStatus::Succeeded,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        source_agent_id: Some("planner.default".to_string()),
        result_summary: Some("done".to_string()),
        join_group: None,
        message: Some("old task".to_string()),
        metadata: None,
        retry_count: 0,
        last_failure_class: None,
        retry_policy: None,
        side_effect_state: None,
        dedupe_key: None,
    };
    save_task_run(config, Some(store), &task)?;

    // save_task_run may have added the task to active_task_ids; clear it for a terminal state.
    let mut workflow = load_workflow_run(config, Some(store), &workflow_id)?
        .expect("workflow should exist");
    workflow.active_task_ids.clear();
    workflow.queued_task_ids.clear();
    workflow.updated_at = chrono::Utc::now().to_rfc3339();
    save_workflow_run(config, Some(store), &workflow)?;

    Ok(workflow_id)
}

#[test]
fn root_agent_spawn_reactivates_terminal_workflow() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let root_session_id = "root-spawn-guard";

    seed_terminal_workflow(
        &config,
        &store,
        root_session_id,
        WorkflowRunStatus::Completed,
    )?;

    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let parent_dir = config.agents_dir.join("planner.default");
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);

    let args = serde_json::json!({
        "agent_id": "coder.default",
        "message": "this should succeed for root planner",
        "async": true
    });

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should create");
    let _guard = runtime.enter();

    let result = registry.execute(
        "agent_spawn",
        &manifest,
        &policy,
        &parent_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args)?,
        Some(root_session_id),
        Some("turn-spawn-guard"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;

    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert_eq!(parsed["ok"].as_bool(), Some(true), "root agent_spawn should succeed on terminal workflow: {}", result);

    let workflow_id = parsed["workflow_id"].as_str().expect("workflow_id should be a string");
    assert!(!workflow_id.is_empty(), "workflow_id should be non-empty");
    let workflow = load_workflow_run(&config, Some(&store), workflow_id)?
        .expect("workflow should exist");
    assert_eq!(
        workflow.status,
        WorkflowRunStatus::Resumable,
        "workflow should be reactivated to Resumable for root planner spawn"
    );
    assert!(
        workflow.reactivated_for_root_spawn,
        "workflow should be flagged as reactivated by root spawn"
    );
    Ok(())
}

#[test]
fn child_agent_spawn_rejected_when_workflow_completed() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let root_session_id = "root-spawn-guard-child";

    seed_terminal_workflow(
        &config,
        &store,
        root_session_id,
        WorkflowRunStatus::Completed,
    )?;

    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let parent_dir = config.agents_dir.join("planner.default");
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);

    let args = serde_json::json!({
        "agent_id": "coder.default",
        "message": "this should fail",
        "async": true
    });

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should create");
    let _guard = runtime.enter();

    let result = registry.execute(
        "agent_spawn",
        &manifest,
        &policy,
        &parent_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args)?,
        Some(&format!("{}/coder-old", root_session_id)),
        Some("turn-spawn-guard-child"),
        Some(&config),
        Some(store.clone()),
        None,
    );

    assert!(result.is_err(), "agent_spawn should fail for terminal workflow from child session");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("terminal"),
        "error should mention terminal workflow: {}",
        err
    );
    Ok(())
}

#[test]
fn child_agent_spawn_allowed_after_root_reactivation() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let root_session_id = "root-spawn-guard-reactivate";

    seed_terminal_workflow(
        &config,
        &store,
        root_session_id,
        WorkflowRunStatus::Completed,
    )?;

    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let parent_dir = config.agents_dir.join("planner.default");
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should create");
    let _guard = runtime.enter();

    // Root reactivates the completed workflow.
    let root_args = serde_json::json!({
        "agent_id": "coder.default",
        "message": "root follow-up spawn",
        "async": true
    });
    let root_result = registry.execute(
        "agent_spawn",
        &manifest,
        &policy,
        &parent_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&root_args)?,
        Some(root_session_id),
        Some("turn-spawn-guard-reactivate-root"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let root_parsed: serde_json::Value = serde_json::from_str(&root_result)?;
    assert_eq!(
        root_parsed["ok"].as_bool(),
        Some(true),
        "root agent_spawn should reactivate workflow"
    );

    // Child session spawn is allowed once the root planner has reactivated the
    // workflow (needed for install flows where the root delegates to agents like
    // agent-factory / specialized_builder).
    let child_args = serde_json::json!({
        "agent_id": "coder.default",
        "message": "child spawn after root reactivation",
        "async": true
    });
    let child_result = registry.execute(
        "agent_spawn",
        &manifest,
        &policy,
        &parent_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&child_args)?,
        Some(&format!("{}/coder-after-reactivate", root_session_id)),
        Some("turn-spawn-guard-reactivate-child"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let child_parsed: serde_json::Value = serde_json::from_str(&child_result)?;
    assert_eq!(
        child_parsed["ok"].as_bool(),
        Some(true),
        "child agent_spawn should succeed after root reactivation: {}",
        child_result
    );
    assert_eq!(
        child_parsed["workflow_id"].as_str(),
        root_parsed["workflow_id"].as_str(),
        "child spawn should share the reactivated workflow"
    );

    let workflow_id = root_parsed["workflow_id"]
        .as_str()
        .expect("workflow_id present");
    let workflow = load_workflow_run(&config, Some(&store), workflow_id)?
        .expect("workflow should exist");
    assert_eq!(
        workflow.status,
        WorkflowRunStatus::Resumable,
        "workflow should stay Resumable after root reactivation"
    );
    Ok(())
}

#[test]
fn workflow_force_complete_rejected_when_workflow_completed() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let root_session_id = "root-force-guard";

    let workflow_id = seed_terminal_workflow(
        &config,
        &store,
        root_session_id,
        WorkflowRunStatus::Completed,
    )?;

    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let parent_dir = config.agents_dir.join("planner.default");
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);

    let args = serde_json::json!({
        "workflow_id": workflow_id,
        "task_id": "task-old",
        "status": "succeeded"
    });

    let result = registry.execute(
        "workflow_force_complete",
        &manifest,
        &policy,
        &parent_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args)?,
        Some(root_session_id),
        Some("turn-force-guard"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;

    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert_eq!(parsed["ok"].as_bool(), Some(false), "force_complete should fail");
    assert_eq!(
        parsed["error"].as_str(),
        Some("workflow_already_completed"),
        "unexpected error response: {}",
        result
    );
    Ok(())
}

#[test]
fn try_complete_workflow_suppresses_pending_notifications() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let root_session_id = "root-complete-suppress";

    let workflow_id = seed_terminal_workflow(
        &config,
        &store,
        root_session_id,
        WorkflowRunStatus::Resumable,
    )?;

    let signal = autonoetic_gateway::scheduler::signal::Signal::ChildStateNotification {
        message: "child done".to_string(),
        notification: ChildStateNotification {
            workflow_id: workflow_id.clone(),
            task_id: "task-old".to_string(),
            child_session_id: format!("{root_session_id}/coder-old"),
            child_status: "succeeded".to_string(),
            failure_class: None,
            install_conflict_detail: None,
            retry_advice: None,
            side_effect_state: None,
            agent_outcome: None,
            summary: Some("done".to_string()),
        },
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    let mut n = NotificationRecord::new(
        "ntf-complete-suppress".to_string(),
        NotificationType::ChildStateNotification,
        root_session_id.to_string(),
        serde_json::to_value(&signal)?,
    );
    n.workflow_id = Some(workflow_id.clone());
    store.create_notification_record(&n)?;

    assert_eq!(
        store.list_notifications_for_session(root_session_id, NotificationStatus::Pending)?.len(),
        1
    );

    let completed = try_complete_workflow(&config, Some(store.as_ref()), root_session_id)?;
    assert!(completed, "workflow should have transitioned to Completed");

    let pending = store.list_notifications_for_session(root_session_id, NotificationStatus::Pending)?;
    assert!(pending.is_empty(), "pending notifications should be suppressed");

    let suppressed = store.list_notifications_for_session(
        root_session_id,
        NotificationStatus::Suppressed,
    )?;
    assert_eq!(suppressed.len(), 1, "notification should be marked Suppressed");
    Ok(())
}

#[test]
fn try_complete_workflow_blocked_by_pending_escalation() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let root_session_id = "root-escalation-block";

    // Seed a workflow in Resumable with a completed task and join satisfied.
    seed_terminal_workflow(
        &config,
        &store,
        root_session_id,
        WorkflowRunStatus::Resumable,
    )?;

    // Create a pending escalation for this root session.
    let escalation = EscalationMessage::new(
        "esc_test_0001".to_string(),
        "ar.test".to_string(),
        "coder.default".to_string(),
        "rev-001".to_string(),
        vec![],
        "Federation verdicts ready for review".to_string(),
        root_session_id.to_string(),
    );
    store.create_escalation(&escalation)?;

    // Workflow should NOT complete while escalation is pending.
    let completed =
        try_complete_workflow(&config, Some(store.as_ref()), root_session_id)?;
    assert!(
        !completed,
        "workflow should not complete while escalation is pending"
    );

    // Verify workflow is still Resumable.
    let wf_id =
        autonoetic_gateway::scheduler::workflow_store::resolve_workflow_id_for_root_session(
            &config,
            root_session_id,
        )?
        .expect("workflow should exist");
    let wf = load_workflow_run(&config, Some(store.as_ref()), &wf_id)?
        .expect("workflow run should exist");
    assert_eq!(
        wf.status,
        WorkflowRunStatus::Resumable,
        "workflow should still be Resumable"
    );

    // Now resolve the escalation.
    store.resolve_escalation(
        "esc_test_0001",
        EscalationStatus::Approved,
        "operator",
        Some("approved"),
    )?;

    // Workflow should now complete.
    let completed =
        try_complete_workflow(&config, Some(store.as_ref()), root_session_id)?;
    assert!(
        completed,
        "workflow should complete after escalation is resolved"
    );

    Ok(())
}

// ── forward compatibility of the lifecycle vocabulary (#1057 review) ────────

/// A transcript row for `root_session_id` carrying `lifecycle_state`.
/// `set_session_lifecycle_state` is an UPDATE, so without a row it silently
/// writes nothing and the read falls through to the pre-migration path — which
/// would make these tests pass without exercising the classifier at all.
fn seed_root_transcript_with_lifecycle(
    store: &GatewayStore,
    root_session_id: &str,
    lifecycle_state: &str,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    store.upsert_session_transcript(&autonoetic_types::causal_chain::SessionTranscriptRecord {
        transcript_id: format!("tid-{root_session_id}"),
        session_id: root_session_id.to_string(),
        root_session_id: root_session_id.to_string(),
        agent_id: "planner.default".to_string(),
        revision_id: None,
        user_id: None,
        started_at: now.clone(),
        ended_at: Some(now),
        status: "completed".to_string(),
        turn_count: 0,
        transcript_handle: None,
        excerpt: None,
        origin_node_id: None,
    })?;
    store.set_session_lifecycle_state(root_session_id, lifecycle_state)?;
    assert_eq!(
        store.get_session_lifecycle_state(root_session_id)?.as_deref(),
        Some(lifecycle_state),
        "the lifecycle_state must actually be stored, or the test proves nothing"
    );
    Ok(())
}

/// A root session whose `lifecycle_state` is `terminated:<reason>` written by a
/// newer gateway must still release its workflow.
///
/// `SessionLifecycleState::FromStr` knows only the reasons this build was
/// compiled with, and adding a `TerminatedReason` is *not* a compile error
/// there (the `_ => Err` arm absorbs it) — so classifying with a bare
/// `parse().ok()` would read a forward-written terminal state as "still
/// running" and park the workflow forever. Terminalness is classified on the
/// `terminated:` prefix instead.
#[test]
fn try_complete_workflow_permits_an_unknown_terminated_reason() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let root_session_id = "root-forward-terminal";

    seed_terminal_workflow(
        &config,
        &store,
        root_session_id,
        WorkflowRunStatus::Resumable,
    )?;
    // A reason this build has never heard of.
    seed_root_transcript_with_lifecycle(&store, root_session_id, "terminated:cancelled")?;
    assert!(
        "terminated:cancelled"
            .parse::<autonoetic_types::agent::SessionLifecycleState>()
            .is_err(),
        "the premise: this build cannot parse the value"
    );

    assert!(
        try_complete_workflow(&config, Some(store.as_ref()), root_session_id)?,
        "a terminated root must release its workflow whatever ended it"
    );
    Ok(())
}

/// A value that is neither known nor terminal-by-prefix carries no signal, so
/// it blocks completion and is surfaced rather than guessed at.
#[test]
fn try_complete_workflow_blocks_on_an_unrecognised_lifecycle() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let root_session_id = "root-unrecognised-lifecycle";

    seed_terminal_workflow(
        &config,
        &store,
        root_session_id,
        WorkflowRunStatus::Resumable,
    )?;
    seed_root_transcript_with_lifecycle(&store, root_session_id, "wedged")?;

    assert!(
        !try_complete_workflow(&config, Some(store.as_ref()), root_session_id)?,
        "an unrecognised lifecycle must not release the workflow"
    );
    Ok(())
}
