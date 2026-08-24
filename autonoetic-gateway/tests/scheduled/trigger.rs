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
        runtime_dir: gateway_dir.clone(),
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
    assert_eq!(event.root_session_id, "root-evolution");
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

    // The WorkflowRun's queued_task_ids must contain the task — the fire must
    // not clobber the run with a stale pre-enqueue snapshot (enqueue_task
    // itself persists the run; a redundant save would drop queued_task_ids).
    let wf = autonoetic_gateway::scheduler::load_workflow_run(
        &config,
        Some(store.as_ref()),
        "sched-sj-trig-001",
    )?
    .expect("workflow run exists");
    assert!(
        wf.queued_task_ids.iter().any(|t| t == &event.task_id),
        "queued_task_ids {:?} must contain the fired task {}",
        wf.queued_task_ids,
        event.task_id
    );

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

/// In-flight guard: after a fire, a follow-up trigger must be flagged as a
/// collision by `inflight_task_for_workflow`. The guard must catch the task
/// both (a) while it still sits in `queued_task_runs` (before the drain
/// promotes it into `task_runs`) and (b) after promotion to a Running TaskRun.
#[test]
fn inflight_guard_detects_queued_and_running_task() -> anyhow::Result<()> {
    let (_temp, store, config) = temp_gateway_store();
    let job = make_job("sj-trig-002", "*/5 * * * *");
    store.create_scheduled_job(&job)?;

    // No in-flight task before any fire.
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

    // (a) Immediately after the fire the task is only in queued_task_runs (the
    // drain has not run). The guard must already see it here, otherwise a
    // second trigger within the same tick window would enqueue a duplicate.
    let queued = store.list_queued_tasks_for_workflow("sched-sj-trig-002")?;
    assert_eq!(queued.len(), 1);
    let inflight_queued = store.inflight_task_for_workflow("sched-sj-trig-002")?;
    assert_eq!(inflight_queued.as_deref(), Some(queued[0].task_id.as_str()));

    // (b) Simulate the drain having promoted the queued task to a Running
    // TaskRun by upserting it, then dequeue the queued copy (the drain does
    // this on promotion).
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
    store.dequeue_queued_task("sched-sj-trig-002", &running.task_id)?;

    // The guard still sees the promoted running task.
    let inflight_running = store.inflight_task_for_workflow("sched-sj-trig-002")?;
    assert_eq!(inflight_running.as_deref(), Some(running.task_id.as_str()));

    Ok(())
}

/// In-flight guard end-to-end via the trigger helper: a second fire without
/// `force` is not possible at this layer (the helper always enqueues); the
/// guard is consulted by the router before calling it. This test documents that
/// two consecutive direct helper calls both enqueue (the guard is the router's
/// responsibility), and that `force` semantics live above the helper.
#[test]
fn helper_always_enqueues_even_when_inflight() -> anyhow::Result<()> {
    let (_temp, store, config) = temp_gateway_store();
    let job = make_job("sj-trig-002b", "*/5 * * * *");
    store.create_scheduled_job(&job)?;

    let _e1 = enqueue_scheduled_job_fire(&config, store.as_ref(), &job, Utc::now(), true, None)?;
    let _e2 = enqueue_scheduled_job_fire(&config, store.as_ref(), &job, Utc::now(), true, None)?;

    // Two distinct tasks enqueued — confirming the helper itself is what the
    // guard protects against, not the helper protecting itself.
    let queued = store.list_queued_tasks_for_workflow("sched-sj-trig-002b")?;
    assert_eq!(queued.len(), 2);
    assert_ne!(queued[0].task_id, queued[1].task_id);

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
