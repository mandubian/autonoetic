use crate::agent::repository::AgentRepository;
use crate::scheduler::cron_parser;
use crate::scheduler::gateway_store::GatewayStore;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::scheduled_job::{ScheduledJob, ScheduledJobStatus};
use std::sync::Arc;

/// Reconcile system agents declared in config with the scheduled_jobs table.
///
/// For each enabled system agent:
/// - If it has a `schedule`, check if an active cron job targeting that agent
///   already exists. If not, create one.
/// - If it has no `schedule`, skip (the agent is declared but not auto-scheduled).
///
/// Called once at gateway startup.
pub fn reconcile_system_agents(
    config: &GatewayConfig,
    store: &Arc<GatewayStore>,
) -> Vec<ReconcileResult> {
    let mut results = Vec::new();
    let repo = AgentRepository::from_config(config);

    for entry in &config.system_agents {
        if !entry.enabled {
            results.push(ReconcileResult {
                agent_id: entry.agent_id.clone(),
                action: ReconcileAction::SkippedDisabled,
                message: "Disabled in config".to_string(),
            });
            continue;
        }

        let gateway_dir = crate::execution::gateway_root_dir(config);

        // Presence check against the promoted revision only (#1136). An agent
        // that exists solely as an ungated `agents_dir` copy is not installed
        // as far as scheduling is concerned.
        let loaded = repo.get_sync_from_store(&entry.agent_id, &gateway_dir, Some(store.as_ref()));

        if let Err(e) = loaded {
            results.push(ReconcileResult {
                agent_id: entry.agent_id.clone(),
                action: ReconcileAction::SkippedMissing,
                message: format!("Agent not found: {}", e),
            });
            continue;
        }

        let Some(schedule) = &entry.schedule else {
            results.push(ReconcileResult {
                agent_id: entry.agent_id.clone(),
                action: ReconcileAction::SkippedNoSchedule,
                message: "No schedule declared (one-shot agent)".to_string(),
            });
            continue;
        };

        let existing = store.list_scheduled_jobs_for_owner(&entry.agent_id, None, None);
        let has_active = existing
            .unwrap_or_default()
            .iter()
            .any(|j| j.target_agent_id == entry.agent_id && j.status == ScheduledJobStatus::Active);

        if has_active {
            results.push(ReconcileResult {
                agent_id: entry.agent_id.clone(),
                action: ReconcileAction::SkippedExists,
                message: "Active cron job already exists".to_string(),
            });
            continue;
        }

        let cron = match cron_parser::parse_schedule(schedule) {
            Ok(c) => c,
            Err(e) => {
                results.push(ReconcileResult {
                    agent_id: entry.agent_id.clone(),
                    action: ReconcileAction::Failed,
                    message: format!("Invalid schedule '{}': {}", schedule, e),
                });
                continue;
            }
        };

        let now = chrono::Utc::now();
        let next_run = match cron_parser::next_occurrence(&cron, now) {
            Some(t) => t,
            None => {
                results.push(ReconcileResult {
                    agent_id: entry.agent_id.clone(),
                    action: ReconcileAction::Failed,
                    message: "No future occurrence for schedule".to_string(),
                });
                continue;
            }
        };

        let agent_ref = match crate::runtime::tools::resolve_target_to_agent_ref(
            &entry.agent_id,
            store.as_ref(),
        ) {
            Ok(r) => r,
            Err(e) => {
                results.push(ReconcileResult {
                    agent_id: entry.agent_id.clone(),
                    action: ReconcileAction::Failed,
                    message: format!("Could not resolve agent: {}", e),
                });
                continue;
            }
        };

        let message = entry
            .message
            .clone()
            .unwrap_or_else(|| format!("Scheduled run for {}", entry.agent_id));

        let job = ScheduledJob {
            job_id: format!("sj-sys-{}", uuid::Uuid::new_v4()),
            owner_agent_id: entry.agent_id.clone(),
            root_session_id: "system".to_string(),
            target_agent_id: agent_ref.agent_id.clone(),
            target_revision_id: agent_ref.revision_id.clone(),
            message,
            metadata_json: Some(
                serde_json::to_string(&serde_json::json!({"system_agent": true})).unwrap(),
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

        match store.create_scheduled_job(&job) {
            Ok(()) => {
                results.push(ReconcileResult {
                    agent_id: entry.agent_id.clone(),
                    action: ReconcileAction::Created,
                    message: format!(
                        "Cron job created: {} (next run: {})",
                        job.cron_expr, job.next_run_at
                    ),
                });
            }
            Err(e) => {
                results.push(ReconcileResult {
                    agent_id: entry.agent_id.clone(),
                    action: ReconcileAction::Failed,
                    message: format!("Failed to create cron job: {}", e),
                });
            }
        }
    }

    results
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ReconcileResult {
    pub agent_id: String,
    pub action: ReconcileAction,
    pub message: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileAction {
    Created,
    SkippedExists,
    SkippedDisabled,
    SkippedMissing,
    SkippedNoSchedule,
    Failed,
}
