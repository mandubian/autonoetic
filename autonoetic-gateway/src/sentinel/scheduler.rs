//! Sentinel scheduled-job registration and execution.
//!
//! Registers two internal cron jobs for the security sentinel at gateway startup:
//!
//! - `sentinel.sweep.full`        — full history sweep (Phase 1 + Phase 2), daily by default.
//! - `sentinel.sweep.incremental` — last-24-h sweep (Phase 1 + Phase 2), every 6 h by default.
//!
//! These jobs are owned by the sentinel pseudo-agent `"security_sentinel"` and
//! are never dispatched to a real agent runtime. The scheduler's
//! `run_due_sentinel_jobs` function is called by the gateway scheduler loop to
//! execute sweeps directly, bypassing the agent session machinery.
//!
//! Job metadata encodes the sweep mode:
//! ```json
//! { "sentinel": true, "mode": "full" }
//! { "sentinel": true, "mode": "incremental" }
//! ```

use anyhow::Result;
use autonoetic_types::config::SentinelConfig;
use autonoetic_types::scheduled_job::{ScheduledJob, ScheduledJobStatus};
use std::path::PathBuf;
use std::sync::Arc;

use crate::scheduler::cron_parser;
use crate::scheduler::gateway_store::GatewayStore;
use super::runner::{SentinelRunner, SweepConfig};

pub const SENTINEL_OWNER: &str = "security_sentinel";
pub const JOB_ID_FULL: &str = "sentinel.sweep.full";
pub const JOB_ID_INCREMENTAL: &str = "sentinel.sweep.incremental";

/// Ensure both sentinel sweep jobs exist in the `scheduled_jobs` table.
///
/// Idempotent — if a job with the given `job_id` already exists and is
/// `Active`, it is left unchanged. Called once at gateway startup.
pub fn ensure_sentinel_scheduled_jobs(
    store: &Arc<GatewayStore>,
    config: &SentinelConfig,
) -> Vec<EnsureJobResult> {
    if !config.enabled {
        return vec![
            EnsureJobResult { job_id: JOB_ID_FULL.to_string(), action: JobAction::SkippedDisabled },
            EnsureJobResult { job_id: JOB_ID_INCREMENTAL.to_string(), action: JobAction::SkippedDisabled },
        ];
    }

    let specs: &[(&str, &str, &str)] = &[
        (JOB_ID_FULL, &config.full_sweep_schedule, "full"),
        (JOB_ID_INCREMENTAL, &config.incremental_sweep_schedule, "incremental"),
    ];

    let mut results = Vec::new();
    let now = chrono::Utc::now();

    // Load existing sentinel jobs once.
    let existing = store.list_scheduled_jobs_for_owner(SENTINEL_OWNER, None, None)
        .unwrap_or_default();

    for (job_id, schedule, mode) in specs {
        let already_active = existing.iter().any(|j| j.job_id == *job_id && j.status == ScheduledJobStatus::Active);
        if already_active {
            results.push(EnsureJobResult { job_id: job_id.to_string(), action: JobAction::SkippedExists });
            continue;
        }

        let cron = match cron_parser::parse_schedule(schedule) {
            Ok(c) => c,
            Err(e) => {
                results.push(EnsureJobResult {
                    job_id: job_id.to_string(),
                    action: JobAction::Failed(format!("Invalid schedule '{}': {}", schedule, e)),
                });
                continue;
            }
        };

        let next_run = match cron_parser::next_occurrence(&cron, now) {
            Some(t) => t,
            None => {
                results.push(EnsureJobResult {
                    job_id: job_id.to_string(),
                    action: JobAction::Failed("No future occurrence for schedule".to_string()),
                });
                continue;
            }
        };

        let metadata = serde_json::json!({ "sentinel": true, "mode": mode });
        let job = ScheduledJob {
            job_id: job_id.to_string(),
            owner_agent_id: SENTINEL_OWNER.to_string(),
            root_session_id: "system".to_string(),
            target_agent_id: SENTINEL_OWNER.to_string(),
            target_revision_id: String::new(),
            message: format!("Sentinel {} sweep", mode),
            metadata_json: Some(metadata.to_string()),
            cron_expr: cron.to_string(),
            timezone: "UTC".to_string(),
            next_run_at: next_run.to_rfc3339(),
            last_run_at: None,
            status: ScheduledJobStatus::Active,
            created_at: now.to_rfc3339(),
            updated_at: now.to_rfc3339(),
            last_error: None,
            generation: 0,
        };

        match store.create_scheduled_job(&job) {
            Ok(()) => results.push(EnsureJobResult { job_id: job_id.to_string(), action: JobAction::Created }),
            Err(e) => results.push(EnsureJobResult {
                job_id: job_id.to_string(),
                action: JobAction::Failed(format!("DB insert error: {}", e)),
            }),
        }
    }

    results
}

/// Execute any due sentinel sweep jobs.
///
/// Called from the gateway scheduler loop on each tick. For each due sentinel
/// job claimed from the store, runs the appropriate sweep synchronously on the
/// current thread (sweeps are fast I/O-bound operations against local SQLite).
///
/// Returns the number of jobs executed.
pub fn run_due_sentinel_jobs(
    store: &Arc<GatewayStore>,
    config: &SentinelConfig,
    agents_dir: Option<&PathBuf>,
) -> usize {
    if !config.enabled {
        return 0;
    }

    let now = chrono::Utc::now();
    let now_str = now.to_rfc3339();

    let due = match store.load_due_scheduled_jobs(&now_str, 32) {
        Ok(jobs) => jobs,
        Err(e) => {
            tracing::warn!(target: "sentinel.scheduler", error = %e, "Failed to load due jobs");
            return 0;
        }
    };

    // Filter to sentinel-owned jobs only.
    let sentinel_due: Vec<_> = due.into_iter().filter(|j| j.owner_agent_id == SENTINEL_OWNER).collect();

    let mut executed = 0;
    for job in &sentinel_due {
        let cron = match cron_parser::parse_schedule(&job.cron_expr) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let next_run = match cron_parser::next_occurrence(&cron, now) {
            Some(t) => t.to_rfc3339(),
            None => now_str.clone(),
        };

        // Claim atomically; skip if lost the race.
        if store.claim_and_advance_due_job(&job.job_id, &now_str, &next_run).is_err() {
            continue;
        }

        let mode = job
            .metadata_json
            .as_deref()
            .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
            .and_then(|v| v["mode"].as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "full".to_string());

        let since = if mode == "incremental" {
            Some((now - chrono::Duration::hours(25)).to_rfc3339())
        } else {
            None
        };

        let sweep_cfg = SweepConfig {
            sentinel_revision_id: config.sentinel_revision_id.clone(),
            since_rfc3339: since,
            ..SweepConfig::default()
        };

        let mut runner = SentinelRunner::new(Arc::clone(store));
        if let Some(dir) = agents_dir {
            runner = runner.with_agents_dir(dir.clone());
        }

        match runner.collect_findings(&sweep_cfg) {
            Ok(raw) => {
                let result = runner.persist_findings(raw);
                tracing::info!(
                    target: "sentinel.scheduler",
                    job_id = %job.job_id,
                    mode = %mode,
                    total = result.total_findings(),
                    errors = result.persist_errors.len(),
                    "Sentinel sweep completed"
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "sentinel.scheduler",
                    job_id = %job.job_id,
                    mode = %mode,
                    error = %e,
                    "Sentinel sweep failed"
                );
            }
        }

        executed += 1;
    }

    executed
}

#[derive(Debug)]
pub struct EnsureJobResult {
    pub job_id: String,
    pub action: JobAction,
}

#[derive(Debug)]
pub enum JobAction {
    Created,
    SkippedExists,
    SkippedDisabled,
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_are_stable() {
        assert_eq!(JOB_ID_FULL, "sentinel.sweep.full");
        assert_eq!(JOB_ID_INCREMENTAL, "sentinel.sweep.incremental");
        assert_eq!(SENTINEL_OWNER, "security_sentinel");
    }

    #[test]
    fn default_sentinel_config_enabled() {
        let cfg = SentinelConfig::default();
        assert!(cfg.enabled);
        assert!(cfg.promotion_gate_enabled);
        assert_eq!(cfg.promotion_gate_timeout_secs, 30);
    }
}
