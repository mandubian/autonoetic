//! Parent-task suspension on pending async children (#845).
//!
//! When an agent spawns async children and ends its turn (the
//! "Sequential / single child" pattern in `docs/AGENTS.md` — Ri-0.14), the
//! gateway MUST keep the parent's workflow task non-terminal so:
//!
//! 1. The workflow join does not fire prematurely.
//! 2. The root planner is not poked into concluding the install pipeline is
//!    done when its own follow-up steps (smoke-test → promote, etc.) never ran.
//! 3. The auto-resume machinery (signal-triggered) can re-wake the parent
//!    task when a child transitions.
//!
//! These tests verify the workflow-layer invariants that the scheduler fix
//! (`run_durable_workflow_task`'s new `suspended_for_child_wait` branch) relies
//! on. They are defense-in-depth: even if the scheduler branch regresses, the
//! join guard must refuse to complete a workflow with a `Paused` (or any other
//! non-terminal) join task.

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::workflow_store::{
    self, check_join_condition, ensure_workflow_for_root_session, load_workflow_run,
    save_task_run, try_complete_workflow,
};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowRunStatus};
use std::sync::Arc;
use tempfile::tempdir;

fn setup() -> anyhow::Result<(tempfile::TempDir, GatewayConfig, Arc<GatewayStore>)> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let parent_dir = agents_dir.join("planner.default");
    std::fs::create_dir_all(&parent_dir)?;
    let config = GatewayConfig {
        agents_dir,
        ..GatewayConfig::default()
    };
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    Ok((temp, config, store))
}

/// Build a TaskRun in the given workflow / session with the supplied status.
fn make_task(
    workflow_id: &str,
    task_id: &str,
    session_id: &str,
    parent_session_id: &str,
    status: TaskRunStatus,
) -> TaskRun {
    TaskRun {
        task_id: task_id.to_string(),
        workflow_id: workflow_id.to_string(),
        agent_id: "agent-factory.default".to_string(),
        session_id: session_id.to_string(),
        parent_session_id: parent_session_id.to_string(),
        status,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        source_agent_id: Some("planner.default".to_string()),
        result_summary: None,
        join_group: None,
        message: Some("delegate + wait".to_string()),
        metadata: None,
        retry_count: 0,
        last_failure_class: None,
        retry_policy: None,
        side_effect_state: None,
        dedupe_key: None,
    }
}

/// `Paused` is the status the scheduler's new branch uses when a parent
/// ends its turn with pending async children. The workflow-layer invariants
/// hinge on these three properties.
#[test]
fn paused_status_properties_block_workflow_completion() {
    assert!(!TaskRunStatus::Paused.is_terminal());
    assert!(!TaskRunStatus::Paused.is_terminal_for_join());
    assert!(TaskRunStatus::Paused.is_resumable());
    // Running → Paused must be a legal transition (it's what the new branch does).
    assert!(TaskRunStatus::Running.try_transition(TaskRunStatus::Paused));
    // And the task can later resume from Paused back to Runnable → Running.
    assert!(TaskRunStatus::Paused.try_transition(TaskRunStatus::Runnable));
    assert!(TaskRunStatus::Runnable.try_transition(TaskRunStatus::Running));
}

/// A join task in `Paused` state MUST keep `check_join_condition` unsatisfied.
/// This is the core invariant: a workflow cannot join-complete while a member
/// of any join group is still waiting on async children.
#[test]
fn paused_join_task_blocks_join_condition() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let root = "root-paused-join";
    let run = ensure_workflow_for_root_session(&config, Some(store.as_ref()), root, None)?;
    let wf_id = run.workflow_id;

    // Two join tasks: one Succeeded (specialized_builder), one Paused
    // (agent-factory, waiting on the smoke-test → promote follow-up its own
    // task spawned and yielded on).
    let parent_task = make_task(
        &wf_id,
        "task-parent",
        &format!("{root}/agent-factory"),
        root,
        TaskRunStatus::Paused,
    );
    let child_task = make_task(
        &wf_id,
        "task-child",
        &format!("{root}/agent-factory/specialized_builder"),
        &format!("{root}/agent-factory"),
        TaskRunStatus::Succeeded,
    );
    save_task_run(&config, Some(store.as_ref()), &parent_task)?;
    save_task_run(&config, Some(store.as_ref()), &child_task)?;

    let mut workflow = load_workflow_run(&config, Some(store.as_ref()), &wf_id)?
        .expect("workflow must exist");
    workflow.join_task_ids = vec!["task-parent".to_string(), "task-child".to_string()];
    workflow.status = WorkflowRunStatus::Resumable;
    workflow_store::save_workflow_run(&config, Some(store.as_ref()), &workflow)?;

    // The join must NOT fire: parent is still in flight (Paused).
    let satisfied = check_join_condition(&config, Some(store.as_ref()), &wf_id)?;
    assert!(
        !satisfied,
        "Paused join task must keep join unsatisfied — otherwise the parent's \
         pending follow-up steps (smoke-test, promote, ...) would be abandoned"
    );

    // And the workflow cannot complete.
    let completed =
        try_complete_workflow(&config, Some(store.as_ref()), &wf_id)?;
    assert!(
        !completed,
        "try_complete_workflow must refuse to complete while a join task is Paused"
    );
    Ok(())
}

/// Sanity check: once the Paused task transitions to Succeeded (the parent
/// ran its follow-up steps after being auto-resumed), the join DOES satisfy.
/// This guarantees the fix doesn't deadlock the workflow forever.
#[test]
fn join_satisfies_after_paused_task_eventually_succeeds() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let root = "root-paused-then-succeed";
    let run = ensure_workflow_for_root_session(&config, Some(store.as_ref()), root, None)?;
    let wf_id = run.workflow_id;

    let parent_task = make_task(
        &wf_id,
        "task-parent",
        &format!("{root}/agent-factory"),
        root,
        TaskRunStatus::Paused,
    );
    let child_task = make_task(
        &wf_id,
        "task-child",
        &format!("{root}/agent-factory/specialized_builder"),
        &format!("{root}/agent-factory"),
        TaskRunStatus::Succeeded,
    );
    save_task_run(&config, Some(store.as_ref()), &parent_task)?;
    save_task_run(&config, Some(store.as_ref()), &child_task)?;

    let mut workflow = load_workflow_run(&config, Some(store.as_ref()), &wf_id)?
        .expect("workflow must exist");
    workflow.join_task_ids = vec!["task-parent".to_string(), "task-child".to_string()];
    workflow.status = WorkflowRunStatus::Resumable;
    workflow_store::save_workflow_run(&config, Some(store.as_ref()), &workflow)?;

    // Before resume: join blocked.
    assert!(!check_join_condition(&config, Some(store.as_ref()), &wf_id)?);

    // Auto-resume fires, parent runs smoke-test → promote, ends turn normally.
    // In production the scheduler walks the task through Paused → Runnable →
    // Running → Succeeded as the parent session re-executes. We replay that
    // transition chain here (Paused → Succeeded directly is forbidden by
    // `try_transition`, which is the point of the guard).
    for next in [
        TaskRunStatus::Runnable,
        TaskRunStatus::Running,
        TaskRunStatus::Succeeded,
    ] {
        workflow_store::update_task_run_status(
            &config,
            Some(store.as_ref()),
            &wf_id,
            "task-parent",
            next,
            if matches!(next, TaskRunStatus::Succeeded) {
                Some("agent installed and promoted".to_string())
            } else {
                None
            },
            None,
            None,
        )?;
    }

    // After resume: join satisfied.
    assert!(
        check_join_condition(&config, Some(store.as_ref()), &wf_id)?,
        "Once the suspended parent task reaches Succeeded, the join must fire"
    );
    Ok(())
}

/// #845 follow-up: when a child task transitions to a terminal state, the
/// scheduler must wake its Paused parent task (transition Paused → Runnable)
/// so the workflow doesn't deadlock.
///
/// Without this wake-up path, Fix 2's `Paused` state has no return path:
/// `process_queued_workflow_tasks` skips Paused (scheduler.rs:1532), and the
/// ChildStateNotification is rerouted to the root planner (router.rs:1093) so
/// the intermediate parent never sees it. The wake-up is implemented in
/// `update_task_run_status` via `wake_paused_child_wait_tasks` — a
/// workflow-scoped, condition-based scan that wakes every parked child-wait
/// task whose own wait set (non-terminal children it spawned) is empty,
/// backstopped by the per-tick janitor `reconcile_paused_child_wait_tasks`.
#[test]
fn child_terminal_transition_wakes_paused_parent_task() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let root = "root-wake-parent";
    let run = ensure_workflow_for_root_session(&config, Some(store.as_ref()), root, None)?;
    let wf_id = run.workflow_id;

    let parent_session = format!("{root}/agent-factory");
    let child_session = format!("{parent_session}/specialized_builder");

    let mut parent_task = make_task(
        &wf_id,
        "task-parent",
        &parent_session,
        root,
        TaskRunStatus::Paused,
    );
    let child_task = make_task(
        &wf_id,
        "task-child",
        &child_session,
        &parent_session,
        TaskRunStatus::Running,
    );
    save_task_run(&config, Some(store.as_ref()), &parent_task)?;
    save_task_run(&config, Some(store.as_ref()), &child_task)?;

    // Drop the mutable borrow; we'll re-load below.
    drop(parent_task);

    // Write a task checkpoint with step = "paused_child_wait" so the wake-up
    // helper recognises this Paused task as parked for child wait (not user
    // input). Without this label, the helper must refuse to wake the task.
    workflow_store::checkpoint_task(
        &config,
        Some(store.as_ref()),
        &wf_id,
        "task-parent",
        "paused_child_wait".to_string(),
        serde_json::json!({"status": "paused", "reason": "awaiting_async_child_completion"}),
    )?;

    // Sanity: parent is Paused before the child completes.
    let before = workflow_store::load_task_run(&config, Some(store.as_ref()), &wf_id, "task-parent")?
        .expect("parent task must exist");
    assert_eq!(before.status, TaskRunStatus::Paused);

    // Child task completes. This must trigger the wake-up.
    workflow_store::update_task_run_status(
        &config,
        Some(store.as_ref()),
        &wf_id,
        "task-child",
        TaskRunStatus::Succeeded,
        Some("candidate revision created".to_string()),
        None,
        None,
    )?;

    // Parent task must have transitioned Paused → Runnable.
    let after = workflow_store::load_task_run(&config, Some(store.as_ref()), &wf_id, "task-parent")?
        .expect("parent task must still exist after child completion");
    assert_eq!(
        after.status,
        TaskRunStatus::Runnable,
        "Paused parent must wake (Paused → Runnable) when its child reaches a terminal state; \
         otherwise the workflow deadlocks because process_queued_workflow_tasks skips Paused"
    );
    Ok(())
}

/// Negative case: a Paused task whose checkpoint is NOT labelled
/// `paused_child_wait` (e.g. `paused` for user input) must NOT be woken by an
/// unrelated child's terminal transition. Its own wake-up path
/// (`interaction_answer.rs`) is responsible for resuming it.
#[test]
fn child_terminal_does_not_wake_paused_user_input_task() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let root = "root-no-wake-user-input";
    let run = ensure_workflow_for_root_session(&config, Some(store.as_ref()), root, None)?;
    let wf_id = run.workflow_id;

    let parent_session = format!("{root}/agent-factory");
    let child_session = format!("{parent_session}/specialized_builder");

    let parent_task = make_task(
        &wf_id,
        "task-parent",
        &parent_session,
        root,
        TaskRunStatus::Paused,
    );
    let child_task = make_task(
        &wf_id,
        "task-child",
        &child_session,
        &parent_session,
        TaskRunStatus::Running,
    );
    save_task_run(&config, Some(store.as_ref()), &parent_task)?;
    save_task_run(&config, Some(store.as_ref()), &child_task)?;

    // Checkpoint step is "paused" (user input), NOT "paused_child_wait".
    workflow_store::checkpoint_task(
        &config,
        Some(store.as_ref()),
        &wf_id,
        "task-parent",
        "paused".to_string(),
        serde_json::json!({"status": "paused", "reason": "awaiting_user_input"}),
    )?;

    // Child completes.
    workflow_store::update_task_run_status(
        &config,
        Some(store.as_ref()),
        &wf_id,
        "task-child",
        TaskRunStatus::Succeeded,
        Some("done".to_string()),
        None,
        None,
    )?;

    // Parent must STAY Paused — its wake-up path is interaction_answer, not
    // child completion.
    let after = workflow_store::load_task_run(&config, Some(store.as_ref()), &wf_id, "task-parent")?
        .expect("parent task must still exist");
    assert_eq!(
        after.status,
        TaskRunStatus::Paused,
        "Paused-for-user-input task must NOT be woken by child terminal transition"
    );
    Ok(())
}

/// Even if the join task is NOT registered in `join_task_ids`, a workflow
/// cannot complete while there is pending active/queued work. This guards
/// against the case where the parent task is Paused but the workflow's
/// `active_task_ids` was somehow not updated.
#[test]
fn active_task_in_workflow_blocks_completion() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let root = "root-active-block";
    let run = ensure_workflow_for_root_session(&config, Some(store.as_ref()), root, None)?;
    let wf_id = run.workflow_id;

    let mut workflow = load_workflow_run(&config, Some(store.as_ref()), &wf_id)?
        .expect("workflow must exist");
    workflow.status = WorkflowRunStatus::Resumable;
    // Simulate a stuck active task (e.g. the paused parent task wasn't dequeued
    // because of a future regression).
    workflow.active_task_ids = vec!["task-stuck".to_string()];
    workflow_store::save_workflow_run(&config, Some(store.as_ref()), &workflow)?;

    let completed =
        try_complete_workflow(&config, Some(store.as_ref()), &wf_id)?;
    assert!(
        !completed,
        "try_complete_workflow must refuse to complete while active_task_ids is non-empty"
    );
    Ok(())
}
