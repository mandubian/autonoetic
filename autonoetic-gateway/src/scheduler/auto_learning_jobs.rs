//! Inject scheduled jobs from `GatewayConfig.auto_learning` when no equivalent
//! `system_agents` entry already schedules the target agent.

use crate::agent::repository::AgentRepository;
use crate::scheduler::cron_parser;
use crate::scheduler::gateway_store::GatewayStore;
use crate::scheduler::system_agents::{ReconcileAction, ReconcileResult};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::scheduled_job::{ScheduledJob, ScheduledJobStatus};
use std::sync::Arc;

/// Synthetic owner for cron rows created from `auto_learning` config.
pub const AUTO_LEARNING_OWNER_ID: &str = "gateway.auto-learning";

const MEMORY_CURATOR_ID: &str = "memory-curator.default";
const EVOLUTION_ORCHESTRATOR_ID: &str = "evolution-orchestrator.default";

/// Daily at 03:00 UTC — evolution runs less frequently than memory curation.
const DEFAULT_EVOLUTION_CRON: &str = "0 3 * * *";

fn system_agent_schedules_target(config: &GatewayConfig, target_agent_id: &str) -> bool {
    config.system_agents.iter().any(|e| {
        e.enabled
            && e.agent_id == target_agent_id
            && e.schedule.is_some()
    })
}

fn has_active_job_for_target(store: &GatewayStore, target_agent_id: &str) -> anyhow::Result<bool> {
    let jobs = store.list_scheduled_jobs_for_root("system")?;
    Ok(jobs.iter().any(|j| {
        j.status == ScheduledJobStatus::Active && j.target_agent_id == target_agent_id
    }))
}

fn create_auto_learning_job(
    store: &Arc<GatewayStore>,
    target_agent_id: &str,
    schedule: &str,
    message: &str,
    metadata_label: &str,
) -> Result<(), String> {
    let cron = cron_parser::parse_schedule(schedule).map_err(|e| {
        format!(
            "Invalid auto-learning schedule '{}' for {}: {}",
            schedule, target_agent_id, e
        )
    })?;

    let now = chrono::Utc::now();
    let next_run = cron_parser::next_occurrence(&cron, now).ok_or_else(|| {
        format!(
            "No future occurrence for auto-learning schedule '{}' ({})",
            schedule, target_agent_id
        )
    })?;

    let agent_ref =
        crate::runtime::tools::resolve_target_to_agent_ref(target_agent_id, store.as_ref())
            .map_err(|e| format!("Could not resolve agent {}: {}", target_agent_id, e))?;

    let job = ScheduledJob {
        job_id: format!("sj-auto-{}", uuid::Uuid::new_v4()),
        owner_agent_id: AUTO_LEARNING_OWNER_ID.to_string(),
        root_session_id: "system".to_string(),
        target_agent_id: agent_ref.agent_id.clone(),
        target_revision_id: agent_ref.revision_id.clone(),
        message: message.to_string(),
        metadata_json: Some(
            serde_json::to_string(&serde_json::json!({
                "auto_learning": true,
                "kind": metadata_label,
            }))
            .unwrap_or_else(|_| "{}".to_string()),
        ),
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

    store
        .create_scheduled_job(&job)
        .map_err(|e| format!("Failed to create scheduled job for {}: {}", target_agent_id, e))
}

/// Ensure memory curator + evolution orchestrator cron rows exist when
/// `auto_learning` is enabled and the operator has not declared overlapping
/// `system_agents` schedules.
pub fn inject_auto_learning_jobs(
    config: &GatewayConfig,
    store: &Arc<GatewayStore>,
) -> Vec<ReconcileResult> {
    let mut results = Vec::new();

    if !config.auto_learning.enabled {
        results.push(ReconcileResult {
            agent_id: AUTO_LEARNING_OWNER_ID.to_string(),
            action: ReconcileAction::SkippedDisabled,
            message: "auto_learning.enabled is false".to_string(),
        });
        return results;
    }

    let gateway_dir = crate::execution::gateway_root_dir(config);
    let repo = AgentRepository::from_config(config);

    // Memory curator — uses `auto_learning.curation_schedule`
    if !system_agent_schedules_target(config, MEMORY_CURATOR_ID) {
        match has_active_job_for_target(store, MEMORY_CURATOR_ID) {
            Ok(true) => {
                results.push(ReconcileResult {
                    agent_id: MEMORY_CURATOR_ID.to_string(),
                    action: ReconcileAction::SkippedExists,
                    message: "Active system cron already targets memory curator".to_string(),
                });
            }
            Ok(false) => {
                let loaded = repo
                    .get_sync_from_store(MEMORY_CURATOR_ID, &gateway_dir, Some(store.as_ref()))
                    .or_else(|_| repo.get_sync(MEMORY_CURATOR_ID));
                if loaded.is_err() {
                    results.push(ReconcileResult {
                        agent_id: MEMORY_CURATOR_ID.to_string(),
                        action: ReconcileAction::SkippedMissing,
                        message: format!(
                            "Agent '{}' not installed — skipping auto-learning cron",
                            MEMORY_CURATOR_ID
                        ),
                    });
                } else {
                    let msg = format!(
                        "Auto-learning memory curation run (schedule: {}). Distill cross-session knowledge.",
                        config.auto_learning.curation_schedule
                    );
                    match create_auto_learning_job(
                        store,
                        MEMORY_CURATOR_ID,
                        &config.auto_learning.curation_schedule,
                        &msg,
                        "memory_curation",
                    ) {
                        Ok(()) => {
                            results.push(ReconcileResult {
                                agent_id: MEMORY_CURATOR_ID.to_string(),
                                action: ReconcileAction::Created,
                                message: format!(
                                    "Auto-learning cron created ({})",
                                    config.auto_learning.curation_schedule
                                ),
                            });
                        }
                        Err(e) => {
                            results.push(ReconcileResult {
                                agent_id: MEMORY_CURATOR_ID.to_string(),
                                action: ReconcileAction::Failed,
                                message: e,
                            });
                        }
                    }
                }
            }
            Err(e) => {
                results.push(ReconcileResult {
                    agent_id: MEMORY_CURATOR_ID.to_string(),
                    action: ReconcileAction::Failed,
                    message: format!("Failed to list scheduled jobs: {}", e),
                });
            }
        }
    } else {
        results.push(ReconcileResult {
            agent_id: MEMORY_CURATOR_ID.to_string(),
            action: ReconcileAction::SkippedExists,
            message: "system_agents already schedules memory-curator.default".to_string(),
        });
    }

    // Evolution orchestrator — fixed daily cadence (independent of curation cron)
    if !system_agent_schedules_target(config, EVOLUTION_ORCHESTRATOR_ID) {
        match has_active_job_for_target(store, EVOLUTION_ORCHESTRATOR_ID) {
            Ok(true) => {
                results.push(ReconcileResult {
                    agent_id: EVOLUTION_ORCHESTRATOR_ID.to_string(),
                    action: ReconcileAction::SkippedExists,
                    message: "Active system cron already targets evolution orchestrator".to_string(),
                });
            }
            Ok(false) => {
                let loaded = repo
                    .get_sync_from_store(
                        EVOLUTION_ORCHESTRATOR_ID,
                        &gateway_dir,
                        Some(store.as_ref()),
                    )
                    .or_else(|_| repo.get_sync(EVOLUTION_ORCHESTRATOR_ID));
                if loaded.is_err() {
                    results.push(ReconcileResult {
                        agent_id: EVOLUTION_ORCHESTRATOR_ID.to_string(),
                        action: ReconcileAction::SkippedMissing,
                        message: format!(
                            "Agent '{}' not installed — skipping evolution cron",
                            EVOLUTION_ORCHESTRATOR_ID
                        ),
                    });
                } else {
                    let msg = concat!(
                        "Run the self-improvement cycle. Call quality_trend_report to see ",
                        "which agents need attention, then orchestrate memory curation and ",
                        "SKILL revisions as appropriate."
                    );
                    match create_auto_learning_job(
                        store,
                        EVOLUTION_ORCHESTRATOR_ID,
                        DEFAULT_EVOLUTION_CRON,
                        msg,
                        "evolution_orchestrator",
                    ) {
                        Ok(()) => {
                            results.push(ReconcileResult {
                                agent_id: EVOLUTION_ORCHESTRATOR_ID.to_string(),
                                action: ReconcileAction::Created,
                                message: format!(
                                    "Evolution orchestrator cron created ({DEFAULT_EVOLUTION_CRON})"
                                ),
                            });
                        }
                        Err(e) => {
                            results.push(ReconcileResult {
                                agent_id: EVOLUTION_ORCHESTRATOR_ID.to_string(),
                                action: ReconcileAction::Failed,
                                message: e,
                            });
                        }
                    }
                }
            }
            Err(e) => {
                results.push(ReconcileResult {
                    agent_id: EVOLUTION_ORCHESTRATOR_ID.to_string(),
                    action: ReconcileAction::Failed,
                    message: format!("Failed to list scheduled jobs: {}", e),
                });
            }
        }
    } else {
        results.push(ReconcileResult {
            agent_id: EVOLUTION_ORCHESTRATOR_ID.to_string(),
            action: ReconcileAction::SkippedExists,
            message: "system_agents already schedules evolution-orchestrator.default".to_string(),
        });
    }

    results
}
