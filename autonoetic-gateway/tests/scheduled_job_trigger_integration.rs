//! Integration tests for the operator-triggered scheduled-job fire
//! (`scheduled_jobs.trigger` / `enqueue_scheduled_job_fire`).
//!
//! These exercise the shared fire helper directly (the same function the cron
//! tick and the `scheduled_jobs.trigger` JSON-RPC method call). The
//! in-flight guard is exercised via `GatewayStore::inflight_task_for_workflow`.

use autonoetic_gateway::scheduler::enqueue_scheduled_job_fire;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::scheduled_job::{ScheduledJob, ScheduledJobStatus};
use chrono::Utc;
use std::sync::Arc;

fn temp_gateway_store() -> (tempfile::TempDir, Arc<GatewayStore>, GatewayConfig) {
    let temp = tempfile::tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let config = GatewayConfig {
        agents_dir,
        ..GatewayConfig::default()
    };
    (temp, store, config)
}

const FAKE_REV: &str =
    "rev_sha256:0000000000000000000000000000000000000000000000000000000000000000";

fn make_job(job_id: &str, cron_expr: &str) -> ScheduledJob {
    let now = Utc::now().to_rfc3339();
    ScheduledJob {
        job_id: job_id.to_string(),
        owner_agent_id: "gateway.auto-learning".to_string(),
        root_session_id: "root-evolution".to_string(),
        target_agent_id: "evolution-orchestrator.default".to_string(),
        target_revision_id: FAKE_REV.to_string(),
        message: "Run the self-improvement cycle.".to_string(),
        metadata_json: None,
        cron_expr: cron_expr.to_string(),
        timezone: "UTC".to_string(),
        next_run_at: now.clone(),
        last_run_at: None,
        status: ScheduledJobStatus::Active,
        created_at: now.clone(),
        updated_at: now,
        last_error: None,
        generation: 0,
    }
}

/// Happy path: firing creates the `sched-{job_id}` WorkflowRun, enqueues a task
/// flagged `manual`, appends a `scheduled_job.triggered` event, and advances
/// `next_run_at` to the next cron occurrence.
#[test]
fn manual_fire_enqueues_task_and_advances_schedule() -> anyhow::Result<()> {
    let (_temp, store, config) = temp_gateway_store();
    // A cron far in the future so the next occurrence is clearly > now.
    let job = make_job("sj-trig-001", "0 3 * * *");
    store.create_scheduled_job(&job)?;

    let before = Utc::now();
    let event = enqueue_scheduled_job_fire(
        &config,
        store.as_ref(),
        &job,
        before,
        /* manual */ true,
        /* next_run_at_override */ None,
    )?;

    // workflow id is the stable sched-{job_id}
    assert_eq!(event.workflow_id, "sched-sj-trig-001");
    assert_eq!(event.job_id, "sj-trig-001");
    assert!(event.task_id.starts_with("task-sj-trig-001-"));

    // A queued task was created with the manual flag set.
    let queued = store.list_queued_tasks_for_workflow("sched-sj-trig-001")?;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].task_id, event.task_id);
    assert_eq!(
        queued[0]
            .metadata
            .as_ref()
            .and_then(|m| m.get("manual"))
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(queued[0].child_session_id, "sched-child-sj-trig-001");

    // A scheduled_job.triggered event was recorded.
    let events = store.list_workflow_events("sched-sj-trig-001")?;
    let triggered = events
        .iter()
        .find(|e| e.event_type == "scheduled_job.triggered")
        .expect("scheduled_job.triggered event appended");
    assert_eq!(triggered.task_id.as_deref(), Some(event.task_id.as_str()));
    assert_eq!(
        triggered
            .payload
            .get("manual")
            .and_then(|v| v.as_bool()),
        Some(true)
    );

    // next_run_at advanced to the next 03:00 UTC after `before`, strictly later.
    let updated = store.get_scheduled_job("sj-trig-001")?.unwrap();
    let next: chrono::DateTime<Utc> = updated.next_run_at.parse()?;
    assert!(next > before);
    assert_eq!(next.format("%M").to_string(), "00");
    assert_eq!(next.format("%H").to_string(), "03");
    assert!(updated.generation > job.generation);

    Ok(())
}

/// In-flight guard: a second fire for a job that already has a Running task is
/// flagged as a collision by `inflight_task_for_workflow`, before any new task
/// is enqueued.
#[test]
fn inflight_guard_detects_running_task() -> anyhow::Result<()> {
    let (_temp, store, config) = temp_gateway_store();
    let job = make_job("sj-trig-002", "*/5 * * * *");
    store.create_scheduled_job(&job)?;

    // First fire — no in-flight task beforehand.
    assert!(store
        .inflight_task_for_workflow("sched-sj-trig-002")?
        .is_none());

    let _event = enqueue_scheduled_job_fire(
        &config,
        store.as_ref(),
        &job,
        Utc::now(),
        true,
        None,
    )?;

    // The queued task is not yet "running" (the drain promotes it), but the
    // guard also treats pending/runnable tasks as in-flight. Simulate the
    // drain having promoted the task to Running by directly upserting a
    // TaskRun.
    let queued = store.list_queued_tasks_for_workflow("sched-sj-trig-002")?;
    let running = autonoetic_types::workflow::TaskRun {
        task_id: queued[0].task_id.clone(),
        workflow_id: "sched-sj-trig-002".to_string(),
        agent_id: queued[0].agent_id.clone(),
        session_id: queued[0].child_session_id.clone(),
        parent_session_id: queued[0].parent_session_id.clone(),
        status: autonoetic_types::workflow::TaskRunStatus::Running,
        created_at: Utc::now().to_rfc3339(),
        updated_at: Utc::now().to_rfc3339(),
        source_agent_id: Some(queued[0].source_agent_id.clone()),
        result_summary: None,
        join_group: None,
        message: Some(queued[0].message.clone()),
        metadata: queued[0].metadata.clone(),
        retry_count: 0,
        last_failure_class: None,
        retry_policy: None,
        side_effect_state: None,
        dedupe_key: None,
    };
    store.upsert_task_run(&running)?;

    // Now the guard sees the running task.
    let inflight = store.inflight_task_for_workflow("sched-sj-trig-002")?;
    assert_eq!(inflight.as_deref(), Some(running.task_id.as_str()));

    Ok(())
}

/// The cron-path caller passes the next occurrence it already computed during
/// claim, and the helper does not perform a redundant `advance_next_run`.
/// The supplied `next_run_at_override` is reflected verbatim in the result.
#[test]
fn cron_path_override_skips_redundant_advance() -> anyhow::Result<()> {
    let (_temp, store, config) = temp_gateway_store();
    let job = make_job("sj-trig-003", "0 3 * * *");
    store.create_scheduled_job(&job)?;

    let supplied = "2099-01-01T03:00:00+00:00";
    let event = enqueue_scheduled_job_fire(
        &config,
        store.as_ref(),
        &job,
        Utc::now(),
        /* manual */ false,
        Some(supplied),
    )?;

    assert_eq!(event.scheduled_for, supplied);
    // generation is unchanged: the helper did not call advance_next_run.
    let updated = store.get_scheduled_job("sj-trig-003")?.unwrap();
    assert_eq!(updated.generation, job.generation);

    Ok(())
}

/// A job whose cron has no future occurrence is rejected with an error rather
/// than silently dropped (manual path has no cron-cancel fallback).
#[test]
fn manual_fire_rejects_unparsable_cron() -> anyhow::Result<()> {
    let (_temp, store, config) = temp_gateway_store();
    let job = make_job("sj-trig-004", "not-a-cron");
    store.create_scheduled_job(&job)?;

    let result = enqueue_scheduled_job_fire(
        &config,
        store.as_ref(),
        &job,
        Utc::now(),
        true,
        None,
    );
    assert!(result.is_err());

    Ok(())
}
