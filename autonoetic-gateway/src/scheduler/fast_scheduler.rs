//! Fast scheduler sidecar — low-latency interval-job dispatcher (issue #8).
//!
//! Runs beside the canonical [`start_background_scheduler`](super::start_background_scheduler).
//! Targets `every N seconds` interval jobs and trips them at sub-second cadence
//! while the DB `claim_and_advance_due_job` call remains the source of truth,
//! preventing double dispatch under races with the canonical loop.
//!
//! Disabled by default (`fast_scheduler.enabled = false`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use autonoetic_types::agent::ExecutionMode;
use chrono::{DateTime, Utc};

use crate::scheduler::{cron_parser, workflow_store};

/// Atomic counters exposed for tests and operator observability.
#[derive(Debug, Default)]
pub struct FastSchedulerStats {
    /// Total candidate jobs returned by the window query (across all ticks).
    pub fast_due_loaded: AtomicU64,
    /// Jobs successfully claimed via `claim_and_advance_due_job`.
    pub fast_claimed: AtomicU64,
    /// Workflow tasks successfully enqueued.
    pub fast_enqueued: AtomicU64,
    /// Claim attempts that returned `None` (job claimed by canonical loop or not yet due).
    pub fast_claim_miss: AtomicU64,
    /// Enqueue failures (backoff applied per-job).
    pub fast_enqueue_failed: AtomicU64,
    /// Total tick durations in milliseconds (sum across all ticks).
    pub fast_tick_duration_ms_total: AtomicU64,
    /// Total ticks executed.
    pub fast_ticks: AtomicU64,
}

impl FastSchedulerStats {
    pub fn snapshot(&self) -> FastSchedulerStatsSnapshot {
        FastSchedulerStatsSnapshot {
            fast_due_loaded: self.fast_due_loaded.load(Ordering::Relaxed),
            fast_claimed: self.fast_claimed.load(Ordering::Relaxed),
            fast_enqueued: self.fast_enqueued.load(Ordering::Relaxed),
            fast_claim_miss: self.fast_claim_miss.load(Ordering::Relaxed),
            fast_enqueue_failed: self.fast_enqueue_failed.load(Ordering::Relaxed),
            fast_tick_duration_ms_total: self.fast_tick_duration_ms_total.load(Ordering::Relaxed),
            fast_ticks: self.fast_ticks.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FastSchedulerStatsSnapshot {
    pub fast_due_loaded: u64,
    pub fast_claimed: u64,
    pub fast_enqueued: u64,
    pub fast_claim_miss: u64,
    pub fast_enqueue_failed: u64,
    pub fast_tick_duration_ms_total: u64,
    pub fast_ticks: u64,
}

/// Entry point spawned from `server::mod::start`. Returns `Ok(())` only on
/// shutdown; the loop is otherwise infinite. When `fast_scheduler.enabled` is
/// false, the future parks forever via `std::future::pending`.
pub async fn start_fast_scheduler(
    execution: Arc<crate::execution::GatewayExecutionService>,
) -> Result<()> {
    let stats = Arc::new(FastSchedulerStats::default());
    start_fast_scheduler_with_stats(execution, stats).await
}

/// Same as [`start_fast_scheduler`] but takes an externally-owned stats handle
/// so tests can observe counters without coupling to the daemon lifecycle.
pub async fn start_fast_scheduler_with_stats(
    execution: Arc<crate::execution::GatewayExecutionService>,
    stats: Arc<FastSchedulerStats>,
) -> Result<()> {
    let config = execution.config();
    let cfg = config.fast_scheduler.clone();
    if !cfg.enabled {
        tracing::info!(
            target: "scheduler::fast",
            "Fast scheduler sidecar disabled (fast_scheduler.enabled=false)"
        );
        std::future::pending::<()>().await;
        unreachable!();
    }

    tracing::info!(
        target: "scheduler::fast",
        tick_millis = cfg.tick_millis,
        window_secs = cfg.window_secs,
        max_due_per_tick = cfg.max_due_per_tick,
        "Fast scheduler sidecar enabled"
    );

    let mut ticker = tokio::time::interval(Duration::from_millis(cfg.tick_millis.max(10)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        if let Err(e) = run_fast_scheduler_tick(execution.clone(), stats.clone()).await {
            tracing::warn!(
                target: "scheduler::fast",
                error = %e,
                "Fast scheduler tick failed"
            );
        }
    }
}

/// Run a single fast-loop tick at `Utc::now()`.
pub async fn run_fast_scheduler_tick(
    execution: Arc<crate::execution::GatewayExecutionService>,
    stats: Arc<FastSchedulerStats>,
) -> Result<()> {
    run_fast_scheduler_tick_at(execution, Utc::now(), stats).await
}

/// Run a single fast-loop tick at the supplied wall-clock instant.
/// Exposed for deterministic testing.
pub async fn run_fast_scheduler_tick_at(
    execution: Arc<crate::execution::GatewayExecutionService>,
    now: DateTime<Utc>,
    stats: Arc<FastSchedulerStats>,
) -> Result<()> {
    let tick_started = Instant::now();
    let config = execution.config();
    let cfg = config.fast_scheduler.clone();
    let store = match execution.gateway_store() {
        Some(s) => s,
        None => return Ok(()),
    };

    let now_rfc = now.to_rfc3339();
    let window_end = now + chrono::Duration::seconds(cfg.window_secs as i64);
    let window_end_rfc = window_end.to_rfc3339();

    let candidates =
        store.load_due_scheduled_jobs_in_window(&window_end_rfc, cfg.max_due_per_tick)?;
    if candidates.is_empty() {
        finish_tick(&stats, tick_started, 0);
        return Ok(());
    }

    stats
        .fast_due_loaded
        .fetch_add(candidates.len() as u64, Ordering::Relaxed);

    for job in candidates {
        let cron = match cron_parser::parse_schedule(&job.cron_expr) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    target: "scheduler::fast",
                    job_id = %job.job_id,
                    cron_expr = %job.cron_expr,
                    error = %e,
                    "Failed to parse cron expression; cancelling job"
                );
                let _ = store.cancel_scheduled_job(&job.job_id);
                continue;
            }
        };

        // Eligibility: fast path handles interval-style schedules only. Cron-style
        // schedules stay on the canonical 1–5s loop where higher tick precision
        // is unnecessary.
        let Some(interval_secs) = cron.interval_seconds else {
            tracing::trace!(
                target: "scheduler::fast",
                job_id = %job.job_id,
                cron_expr = %job.cron_expr,
                "Skipping non-interval schedule on fast path"
            );
            continue;
        };

        // Defense-in-depth: re-check the sub-10s script-mode guardrail at
        // dispatch boundary. The cron tool enforces this at creation time
        // (see runtime/tools/scheduler.rs); re-enforcing here protects
        // against direct DB writes and against a target whose manifest was
        // mutated after job creation.
        if interval_secs < 10 && !target_is_script_mode(&config, store.as_ref(), &job.target_agent_id) {
            tracing::warn!(
                target: "scheduler::fast",
                job_id = %job.job_id,
                target_agent_id = %job.target_agent_id,
                interval_secs,
                "Sub-10s schedule lost its script-mode target after creation; cancelling job"
            );
            let _ = store.cancel_scheduled_job(&job.job_id);
            continue;
        }

        let next_occurrence = cron_parser::next_occurrence(&cron, now);
        let next_run_at = match next_occurrence {
            Some(n) => n.to_rfc3339(),
            None => {
                tracing::warn!(
                    target: "scheduler::fast",
                    job_id = %job.job_id,
                    "No future occurrence found for cron expression; cancelling job"
                );
                let _ = store.cancel_scheduled_job(&job.job_id);
                continue;
            }
        };

        let claimed = match store.claim_and_advance_due_job(&job.job_id, &now_rfc, &next_run_at) {
            Ok(Some(j)) => j,
            Ok(None) => {
                stats.fast_claim_miss.fetch_add(1, Ordering::Relaxed);
                tracing::trace!(
                    target: "scheduler::fast",
                    job_id = %job.job_id,
                    "Could not claim scheduled job (canonical loop won, not yet due, or paused)"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    target: "scheduler::fast",
                    job_id = %job.job_id,
                    error = %e,
                    "Failed to claim scheduled job"
                );
                continue;
            }
        };

        stats.fast_claimed.fetch_add(1, Ordering::Relaxed);

        tracing::info!(
            target: "scheduler::fast",
            job_id = %claimed.job_id,
            root_session_id = %claimed.root_session_id,
            owner_agent_id = %claimed.owner_agent_id,
            target_agent_id = %claimed.target_agent_id,
            interval_secs,
            "Triggering scheduled job (fast path)"
        );

        let workflow_id = format!("sched-{}", &claimed.job_id);
        let task_id = format!(
            "task-{}-{}",
            &claimed.job_id,
            &uuid::Uuid::new_v4().to_string()[..8]
        );

        let _run =
            match workflow_store::load_workflow_run(&config, Some(store.as_ref()), &workflow_id) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    let new_run = autonoetic_types::workflow::WorkflowRun {
                        workflow_id: workflow_id.clone(),
                        root_session_id: claimed.root_session_id.clone(),
                        lead_agent_id: claimed.owner_agent_id.clone(),
                        status: autonoetic_types::workflow::WorkflowRunStatus::Active,
                        created_at: now_rfc.clone(),
                        updated_at: now_rfc.clone(),
                        active_task_ids: Vec::new(),
                        queued_task_ids: Vec::new(),
                        join_policy: autonoetic_types::workflow::JoinPolicy::AllOf,
                        join_task_ids: Vec::new(),
                    };
                    if let Err(e) =
                        workflow_store::save_workflow_run(&config, Some(store.as_ref()), &new_run)
                    {
                        tracing::warn!(
                            target: "scheduler::fast",
                            workflow_id = %workflow_id,
                            error = %e,
                            "Failed to persist new workflow run for scheduled job"
                        );
                        continue;
                    }
                    new_run
                }
                Err(e) => {
                    tracing::warn!(
                        target: "scheduler::fast",
                        workflow_id = %workflow_id,
                        error = %e,
                        "Failed to load workflow run for scheduled job"
                    );
                    continue;
                }
            };

        let queued = autonoetic_types::workflow::QueuedTaskRun {
            task_id: task_id.clone(),
            workflow_id: workflow_id.clone(),
            agent_id: format!("{}@{}", claimed.target_agent_id, claimed.target_revision_id),
            message: claimed.message.clone(),
            child_session_id: format!("sched-child-{}", &claimed.job_id),
            parent_session_id: claimed.root_session_id.clone(),
            source_agent_id: claimed.owner_agent_id.clone(),
            metadata: Some(serde_json::json!({
                "scheduled_job_id": claimed.job_id,
                "scheduled_next_run_at": next_run_at.clone(),
                "fast_path": true,
            })),
            join_group: None,
            blocks_planner: false,
            enqueued_at: now_rfc.clone(),
        };

        if let Err(e) = workflow_store::enqueue_task(&config, Some(store.as_ref()), &queued) {
            stats.fast_enqueue_failed.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                target: "scheduler::fast",
                job_id = %claimed.job_id,
                task_id = %task_id,
                error = %e,
                "Failed to enqueue scheduled job task; recording error for backoff"
            );
            let backoff_secs = 60;
            let retry_at =
                (chrono::Utc::now() + chrono::Duration::seconds(backoff_secs)).to_rfc3339();
            let _ = store.advance_next_run(
                &claimed.job_id,
                &retry_at,
                None,
                Some(&format!("Enqueue failed: {}", e)),
            );
            continue;
        }

        stats.fast_enqueued.fetch_add(1, Ordering::Relaxed);

        let trigger_event = autonoetic_types::workflow::WorkflowEventRecord {
            event_id: format!("wevt-sched-fast-{}", &task_id),
            workflow_id: workflow_id.clone(),
            event_type: "scheduled_job.triggered".to_string(),
            task_id: Some(task_id.clone()),
            agent_id: Some(claimed.target_agent_id.clone()),
            payload: serde_json::json!({
                "job_id": claimed.job_id,
                "owner_agent_id": claimed.owner_agent_id,
                "scheduled_for": next_run_at,
                "fast_path": true,
            }),
            occurred_at: now_rfc.clone(),
        };
        let _ = workflow_store::append_workflow_event(&config, Some(store.as_ref()), &trigger_event);
    }

    let elapsed_ms = tick_started.elapsed().as_millis() as u64;
    finish_tick(&stats, tick_started, elapsed_ms);
    Ok(())
}

fn finish_tick(stats: &FastSchedulerStats, _tick_started: Instant, elapsed_ms: u64) {
    stats
        .fast_tick_duration_ms_total
        .fetch_add(elapsed_ms, Ordering::Relaxed);
    stats.fast_ticks.fetch_add(1, Ordering::Relaxed);
}

/// Resolve a target agent's `execution_mode` via the repository alias.
/// Returns `false` (i.e. "not script mode") whenever the target cannot be
/// resolved — this is the fail-closed direction for the sub-10s guardrail.
fn target_is_script_mode(
    config: &autonoetic_types::config::GatewayConfig,
    store: &crate::scheduler::gateway_store::GatewayStore,
    target_agent_id: &str,
) -> bool {
    let repo = crate::agent::repository::AgentRepository::from_config(config);
    let gateway_dir = config.agents_dir.join(".gateway");
    match repo.get_sync_from_store(target_agent_id, &gateway_dir, Some(store)) {
        Ok(loaded) => matches!(loaded.manifest.execution_mode, ExecutionMode::Script),
        Err(_) => match repo.get_sync(target_agent_id) {
            Ok(loaded) => matches!(loaded.manifest.execution_mode, ExecutionMode::Script),
            Err(_) => false,
        },
    }
}
