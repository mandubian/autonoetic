//! Gateway-owned background scheduler.
//!
//! This module has been split by domain responsibility:
//! - [`crate::scheduler::decision`] - Wake-decision logic
//! - [`crate::scheduler::store`] - Persistence helpers  
//! - [`crate::scheduler::approval`] - Approval resolution
//! - [`crate::scheduler::runner`] - Side-effecting execution
//!
//! The main entry points remain in this file for backwards compatibility.

use chrono::{DateTime, Duration, Utc};
use std::sync::Arc;

use crate::runtime::continuation;

pub mod approval;
pub mod approval_hardening;
pub mod approval_similarity;
pub mod cron_parser;
pub mod decision;
pub mod eval_runner;
pub mod gateway_store;
pub mod hooks;
pub mod runner;
pub mod signal;
pub mod store;
pub mod system_agents;
pub mod workflow_causal;
pub mod workflow_store;

pub use approval::*;
pub use decision::*;
pub use gateway_store::*;
pub use runner::*;
pub use signal::*;
pub use store::*;
pub use workflow_causal::*;
pub use workflow_store::*;

pub async fn start_background_scheduler(
    execution: Arc<crate::execution::GatewayExecutionService>,
) -> anyhow::Result<()> {
    let config = execution.config();
    if !config.background_scheduler_enabled {
        tracing::info!("Background scheduler disabled");
        std::future::pending::<()>().await;
        unreachable!();
    }

    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
        config.background_tick_secs.max(1),
    ));
    loop {
        ticker.tick().await;
        if let Err(e) = run_scheduler_tick(execution.clone()).await {
            tracing::warn!(error = %e, "Background scheduler tick failed");
        }
    }
}

pub async fn run_scheduler_tick(
    execution: Arc<crate::execution::GatewayExecutionService>,
) -> anyhow::Result<()> {
    run_scheduler_tick_at(execution, Utc::now()).await
}

async fn run_scheduler_tick_at(
    execution: Arc<crate::execution::GatewayExecutionService>,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    // Process pending notifications
    if let Some(store) = execution.gateway_store() {
        if let Err(e) = process_pending_notifications(execution.clone(), store.as_ref()).await {
            tracing::warn!(error = %e, "Failed to process pending notifications");
        }
        // Cleanup stale notifications (e.g., older than 24h)
        let _ = store.cleanup_stale_notifications(24);
        if let Err(e) = store.prune_expired_grants() {
            tracing::warn!(error = %e, "Failed to prune expired session approval grants");
        }

        // R++8: Check for sessions exceeding sandbox escape thresholds
        let degrade_threshold = execution.config().escape_attempt_degrade_threshold;
        let emergency_threshold = execution.config().escape_attempt_emergency_threshold;
        if emergency_threshold > 0 {
            if let Ok(sessions) = store.sessions_exceeding_escape_threshold(emergency_threshold) {
                for (sid, root_sid, count) in sessions {
                    if !execution.is_session_degraded(&sid).await {
                        let reason = format!(
                            "sandbox escape attempts ({}) exceeded emergency threshold ({}) (R++8)",
                            count, emergency_threshold
                        );
                        if let Err(e) = execution
                            .emergency_stop_from_security_policy(&root_sid, &reason)
                            .await
                        {
                            tracing::warn!(
                                error = %e,
                                root_session_id = %root_sid,
                                "Failed to trigger emergency stop for escape threshold breach"
                            );
                        }
                    }
                }
            }
        }
        if degrade_threshold > 0 && (emergency_threshold == 0 || degrade_threshold < emergency_threshold) {
            if let Ok(sessions) = store.sessions_exceeding_escape_threshold(degrade_threshold) {
                for (sid, _root_sid, count) in sessions {
                    if !execution.is_session_degraded(&sid).await {
                        let reason = format!(
                            "sandbox escape attempts ({}) exceeded degradation threshold ({}) (R++8)",
                            count, degrade_threshold
                        );
                        if let Err(e) = execution.degrade_session(&sid, &reason).await {
                            tracing::warn!(
                                error = %e,
                                session_id = %sid,
                                "Failed to degrade session for escape threshold breach"
                            );
                        }
                    }
                }
            }
        }
    }

    let config = execution.config();

    // Process due scheduled jobs before workflow drains (may enqueue follow-up work).
    if let Err(e) = process_due_scheduled_jobs(execution.clone(), now).await {
        tracing::warn!(error = %e, "Failed to process due scheduled jobs");
    }

    // Drain runnable + queued workflow work *before* background-agent wakes. `handle_due_wake`
    // may await a full reasoning `spawn_agent_once` turn (minutes of LLM); when it runs first,
    // async `agent_spawn` children starve with no queue processing until it returns.
    if let Err(e) = process_runnable_workflow_tasks(execution.clone()).await {
        tracing::warn!(error = %e, "Failed to process runnable workflow tasks");
    }
    if let Err(e) = process_queued_workflow_tasks(execution.clone()).await {
        tracing::warn!(error = %e, "Failed to process queued workflow tasks");
    }

    let repo = crate::agent::AgentRepository::from_config(&config);
    let gateway_dir = crate::execution::gateway_root_dir(&config);
    let mut loaded_agents: Vec<crate::agent::LoadedAgent> = Vec::new();

    if let Some(gateway_store) = execution.gateway_store() {
        let alias_rows = gateway_store.list_agent_aliases(None)?;
        for alias in alias_rows {
            let loaded = repo
                .load_from_revision_dir(&gateway_dir, &alias.agent_id, &alias.revision_id)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "Failed to load agent '{}' from active alias revision '{}': {}",
                        alias.agent_id,
                        alias.revision_id,
                        e
                    )
                })?;
            loaded_agents.push(loaded);
        }
    }

    let mut admitted = 0usize;

    for loaded in loaded_agents {
        if admitted >= config.max_background_due_per_tick.max(1) {
            break;
        }

        let Some(background) = loaded.manifest.background.clone() else {
            continue;
        };
        if !background.enabled {
            continue;
        }

        let policy = crate::policy::PolicyEngine::new(loaded.manifest.clone());
        let Some((cap_min_interval, allow_reasoning)) = policy.background_reevaluation_limits()
        else {
            continue;
        };

        let session_id = decision::background_session_id(&loaded.manifest.agent.id);
        let effective_interval =
            decision::effective_interval_secs(&config, &background, cap_min_interval);
        let state_path = store::background_state_path(&config, &loaded.manifest.agent.id);
        let mut state =
            store::load_background_state(&state_path, &loaded.manifest.agent.id, &session_id)?;
        if state.next_due_at.is_none() {
            state.next_due_at =
                Some((now + Duration::seconds(effective_interval as i64)).to_rfc3339());
            store::save_background_state(&state_path, &state)?;
        }

        let reevaluation =
            crate::runtime::reevaluation_state::load_reevaluation_state(&loaded.dir)?;
        let reason = decision::should_wake(
            &config,
            &loaded.manifest.agent.id,
            &session_id,
            &background,
            &state,
            &reevaluation,
            execution.gateway_store().as_deref(),
            now,
        )?;
        decision::log_should_wake(
            &config,
            &session_id,
            &loaded.manifest.agent.id,
            &reason,
            effective_interval,
        );

        let Some(reason) = reason else {
            continue;
        };
        admitted += 1;

        runner::handle_due_wake(
            execution.clone(),
            &loaded.manifest.agent.id,
            &loaded.dir,
            &background,
            allow_reasoning,
            effective_interval,
            &session_id,
            state,
            reevaluation,
            reason,
            now,
        )
        .await?;
    }

    // Fail tasks that have been AwaitingApproval longer than the configured timeout.
    if let Err(e) = check_approval_timeouts(execution.clone()).await {
        tracing::warn!(error = %e, "Failed to check approval timeouts");
    }

    // Detect and resolve tasks stuck in Running state (child session completed but task status not updated).
    if let Err(e) = check_stuck_running_tasks(execution.clone()).await {
        tracing::warn!(error = %e, "Failed to check stuck running tasks");
    }

    // Orphan-child reaper: cancel children of terminated parent sessions (R+12)
    if let Err(e) = reap_orphaned_sessions(execution.clone()).await {
        tracing::warn!(error = %e, "Failed to reap orphaned sessions");
    }

    // Resume standalone sessions whose user interaction has been answered
    if let Err(e) = resume_answered_standalone_interactions(execution).await {
        tracing::warn!(error = %e, "Failed to resume answered standalone interactions");
    }

    Ok(())
}

/// Fail workflow tasks that have been stuck in `AwaitingApproval` longer than
/// `config.approval_timeout_secs`. When a task times out its continuation file
/// is deleted so stale disk state doesn't accumulate.
async fn check_approval_timeouts(
    execution: Arc<crate::execution::GatewayExecutionService>,
) -> anyhow::Result<()> {
    let config = execution.config();
    let timeout_secs = config.approval_timeout_secs;
    if timeout_secs == 0 {
        return Ok(()); // disabled
    }

    let store = execution.gateway_store();
    let store = store.as_deref();

    let workflows_root = workflow_store::workflows_root(&config).join("runs");
    if !workflows_root.is_dir() {
        return Ok(());
    }

    let now = chrono::Utc::now();

    for entry in std::fs::read_dir(&workflows_root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let wf_id = entry.file_name().to_string_lossy().to_string();
        let tasks = workflow_store::list_task_runs_for_workflow(&config, store, &wf_id)?;

        for task in tasks {
            if task.status != autonoetic_types::workflow::TaskRunStatus::AwaitingApproval {
                continue;
            }

            // Check the continuation file's `suspended_at` timestamp.
            let cont = crate::runtime::continuation::load_continuation(&config, &task.task_id)
                .ok()
                .flatten();
            let suspended_at = cont.as_ref().and_then(|c| {
                chrono::DateTime::parse_from_rfc3339(&c.suspended_at)
                    .ok()
                    .map(|dt| dt.with_timezone(&chrono::Utc))
            });

            let timed_out = match suspended_at {
                Some(ts) => (now - ts).num_seconds() as u64 > timeout_secs,
                None => false, // no continuation → not managed by new model
            };

            if timed_out {
                tracing::warn!(
                    target: "workflow",
                    workflow_id = %wf_id,
                    task_id = %task.task_id,
                    timeout_secs = timeout_secs,
                    "Approval timeout expired; marking task as failed (continuation preserved)"
                );

                let reason = "Approval timed out".to_string();
                // Mark as Failed but don't crash — the session can still be resumed
                // if the operator approves later. The continuation file is preserved.
                let _ = workflow_store::update_task_run_status(
                    &config,
                    store,
                    &wf_id,
                    &task.task_id,
                    autonoetic_types::workflow::TaskRunStatus::Failed,
                    Some(reason.clone()),
                    None,
                    None,
                );
                let _ = workflow_store::checkpoint_task(
                    &config,
                    store,
                    &wf_id,
                    &task.task_id,
                    "approval_timeout".to_string(),
                    serde_json::json!({
                        "reason": reason,
                        "timeout_secs": timeout_secs,
                    }),
                );
                // DO NOT delete the continuation file — it can be resumed if
                // the operator approves later (R-2.11, R-7.11). Just emit workflow event.
                let timeout_event = autonoetic_types::workflow::WorkflowEventRecord {
                    event_id: format!("wevt-approval-t-{}", &task.task_id),
                    workflow_id: wf_id.clone(),
                    event_type: "task.approval_timeout".to_string(),
                    task_id: Some(task.task_id.clone()),
                    agent_id: None,
                    payload: serde_json::json!({
                        "reason": reason,
                        "timeout_secs": timeout_secs,
                    }),
                    occurred_at: chrono::Utc::now().to_rfc3339(),
                };
                let _ = workflow_store::append_workflow_event(&config, store, &timeout_event);
                let _ = workflow_store::dequeue_task(&config, store, &wf_id, &task.task_id);

                // Record timeout in session report so overview stays consistent.
                if let Some(ref c) = cont {
                    let gateway_dir = config.agents_dir.join(".gateway");
                    if let Ok(mut report) = crate::runtime::session_report::SessionReportWriter::open(
                        &gateway_dir,
                        &task.session_id,
                        &task.agent_id,
                    ) {
                        let _ = report.record_approval_resolved(
                            &c.approval_request_id,
                            "timed_out",
                            &format!("Approval timed out after {}s (continuation preserved)", timeout_secs),
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

async fn check_stuck_running_tasks(
    execution: Arc<crate::execution::GatewayExecutionService>,
) -> anyhow::Result<()> {
    let config = execution.config();
    let stale_after_secs = config.stuck_task_timeout_secs.unwrap_or(600);
    if stale_after_secs == 0 {
        return Ok(());
    }

    let store = execution.gateway_store();
    let store = store.as_deref();
    let gateway_dir = crate::execution::gateway_root_dir(&config);

    let workflows_root = workflow_store::workflows_root(&config).join("runs");
    if !workflows_root.is_dir() {
        return Ok(());
    }

    let now = chrono::Utc::now();

    for entry in std::fs::read_dir(&workflows_root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let wf_id = entry.file_name().to_string_lossy().to_string();
        let tasks = workflow_store::list_task_runs_for_workflow(&config, store, &wf_id)?;

        for task in tasks {
            if task.status != autonoetic_types::workflow::TaskRunStatus::Running {
                continue;
            }

            let updated_at = chrono::DateTime::parse_from_rfc3339(&task.updated_at)
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc));

            let elapsed_secs = updated_at
                .map(|ts| (now - ts).num_seconds() as u64)
                .unwrap_or(0);

            if elapsed_secs < stale_after_secs {
                continue;
            }

            let mut evidence = Vec::new();
            let mut session_completed = false;

            if !task.session_id.is_empty() {
                let session_dir = gateway_dir.join("sessions").join(&task.session_id);
                if session_dir.exists() {
                    let has_manifest = session_dir.join("manifest.json").exists();
                    let has_digest = session_dir.join("digest.md").exists();

                    if has_manifest {
                        evidence.push("session manifest exists".to_string());
                    }
                    if has_digest {
                        evidence.push("session digest exists".to_string());
                    }

                    if let Ok(content) = std::fs::read_to_string(session_dir.join("manifest.json"))
                    {
                        if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&content) {
                            if let Some(vis) = manifest.get("visibility") {
                                if let Some(status) = vis.get("status") {
                                    if let Some(s) = status.as_str() {
                                        if s == "completed" || s == "done" {
                                            session_completed = true;
                                            evidence.push(
                                                "session manifest shows completed status"
                                                    .to_string(),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if has_digest {
                        if let Ok(digest) = std::fs::read_to_string(session_dir.join("digest.md")) {
                            if digest.contains("Session summary")
                                || digest.contains("jsonrpc_spawn_complete")
                            {
                                session_completed = true;
                                evidence
                                    .push("session digest contains completion markers".to_string());
                            }
                        }
                    }
                } else {
                    evidence.push("session directory does not exist".to_string());
                }
            }

            if let Ok(Some(checkpoint)) =
                workflow_store::load_task_checkpoint(&config, store, &wf_id, &task.task_id)
            {
                evidence.push(format!(
                    "checkpoint exists (step: {}, version: {})",
                    checkpoint.step, checkpoint.version
                ));
            }

            if let Ok(content_store) =
                crate::runtime::content_store::ContentStore::new(&gateway_dir)
            {
                if !task.session_id.is_empty() {
                    let implicit_name = format!("impl_{}", task.task_id);
                    if let Ok(names) = content_store.list_names(&task.session_id) {
                        if names.contains(&implicit_name) {
                            session_completed = true;
                            evidence.push("implicit artifact exists (impl_task)".to_string());
                        }
                    }
                }
            }

            if !session_completed && evidence.is_empty() {
                evidence.push("no evidence found — proceeding based on elapsed time".to_string());
            }

            tracing::warn!(
                target: "workflow",
                task_id = %task.task_id,
                workflow_id = %wf_id,
                elapsed_secs = elapsed_secs,
                evidence = ?evidence,
                "Stuck running task detected; force-completing as Succeeded"
            );

            let result_summary = format!(
                "Auto-resolved stuck task: Succeeded (elapsed: {}s, evidence: {})",
                elapsed_secs,
                evidence.join("; ")
            );

            let _ = workflow_store::update_task_run_status(
                &config,
                store,
                &wf_id,
                &task.task_id,
                autonoetic_types::workflow::TaskRunStatus::Succeeded,
                Some(result_summary),
                None,
                None,
            );

            let _ = workflow_store::checkpoint_task(
                &config,
                store,
                &wf_id,
                &task.task_id,
                "stuck_auto_resolved".to_string(),
                serde_json::json!({
                    "status": "succeeded",
                    "evidence": evidence,
                    "session_completed": session_completed,
                    "elapsed_secs": elapsed_secs,
                }),
            );

            let _ = workflow_store::dequeue_task(&config, store, &wf_id, &task.task_id);

            let event = autonoetic_types::workflow::WorkflowEventRecord {
                event_id: format!("wevt-stuck-t-{}", &task.task_id),
                workflow_id: wf_id.clone(),
                event_type: "task.stuck_resolved".to_string(),
                task_id: Some(task.task_id.clone()),
                agent_id: Some(task.agent_id.clone()),
                payload: serde_json::json!({
                    "evidence": evidence,
                    "session_completed": session_completed,
                    "elapsed_secs": elapsed_secs,
                }),
                occurred_at: chrono::Utc::now().to_rfc3339(),
            };
            let _ = workflow_store::append_workflow_event(&config, store, &event);
        }
    }

    Ok(())
}

pub async fn reap_orphaned_sessions(
    execution: Arc<crate::execution::GatewayExecutionService>,
) -> anyhow::Result<()> {
    let store = match execution.gateway_store() {
        Some(s) => s,
        None => return Ok(()),
    };

    let orphans = store.find_orphaned_sessions()?;
    if orphans.is_empty() {
        return Ok(());
    }

    let config = execution.config();
    let now = chrono::Utc::now();
    let now_rfc = now.to_rfc3339();
    let event_seq_base = now.timestamp_millis().max(0) as u64;

    let workflows_root = crate::scheduler::workflow_store::workflows_root(&config).join("runs");
    let mut workflow_dirs: Vec<std::path::PathBuf> = Vec::new();
    if workflows_root.is_dir() {
        for entry in std::fs::read_dir(&workflows_root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                workflow_dirs.push(entry.path());
            }
        }
    }

    for (idx, (child_session_id, parent_session_id, root_session_id, agent_id)) in
        orphans.into_iter().enumerate()
    {
        tracing::info!(
            child_session_id = %child_session_id,
            parent_session_id = %parent_session_id,
            root_session_id = %root_session_id,
            agent_id = %agent_id,
            "Reaping orphaned session (R+12)"
        );

        let _ = store.finalize_session_transcript(&child_session_id, &now_rfc, "failed");

        let event = autonoetic_types::causal_chain::CausalEventRecord {
            event_id: uuid::Uuid::new_v4().to_string(),
            agent_id: agent_id.clone(),
            session_id: child_session_id.clone(),
            turn_id: None,
            event_seq: event_seq_base + idx as u64,
            timestamp: now_rfc.clone(),
            category: "session".to_string(),
            action: "parent_terminated".to_string(),
            status: "error".to_string(),
            enforced_rules: vec!["R+12".to_string()],
            target: Some(parent_session_id.clone()),
            payload: Some(
                serde_json::json!({
                    "parent_session_id": parent_session_id,
                    "root_session_id": root_session_id,
                    "reason": "parent_session_ended",
                })
                .to_string(),
            ),
            payload_ref: None,
            evidence_ref: None,
            reason: Some("Orphan-child reaper: parent session terminated".to_string()),
        };
        let _ = store.create_causal_event(&event);

        for wf_dir in &workflow_dirs {
            let tasks_dir = wf_dir.join("tasks");
            if !tasks_dir.is_dir() {
                continue;
            }
            let wf_id = match wf_dir.file_name().and_then(|n| n.to_str()) {
                Some(id) => id.to_string(),
                None => continue,
            };
            let tasks = match crate::scheduler::workflow_store::list_task_runs_for_workflow(
                &config,
                Some(store.as_ref()),
                &wf_id,
            ) {
                Ok(t) => t,
                Err(_) => continue,
            };
            for mut task in tasks {
                if task.session_id != child_session_id {
                    continue;
                }
                let is_terminal = matches!(
                    task.status,
                    autonoetic_types::workflow::TaskRunStatus::Succeeded
                        | autonoetic_types::workflow::TaskRunStatus::Failed
                        | autonoetic_types::workflow::TaskRunStatus::Cancelled
                        | autonoetic_types::workflow::TaskRunStatus::Aborted
                );
                if is_terminal {
                    continue;
                }
                task.status = autonoetic_types::workflow::TaskRunStatus::Cancelled;
                task.updated_at = now_rfc.clone();
                task.result_summary = Some(
                    "Cancelled by orphan-child reaper (R+12): parent session terminated"
                        .to_string(),
                );
                let _ = crate::scheduler::workflow_store::save_task_run(
                    &config,
                    Some(store.as_ref()),
                    &task,
                );
                crate::scheduler::workflow_store::dequeue_task(
                    &config,
                    Some(store.as_ref()),
                    &task.workflow_id,
                    &task.task_id,
                )
                .ok();
            }
        }

        let sanitized = crate::runtime::checkpoint::sanitize_path_component(&child_session_id);
        let checkpoint_dir = crate::execution::gateway_root_dir(&config)
            .join("checkpoints")
            .join(&sanitized);
        if checkpoint_dir.is_dir() {
            let _ = std::fs::remove_dir_all(&checkpoint_dir);
        }
    }

    Ok(())
}

pub fn workflow_task_heartbeat_interval_secs(
    config: &autonoetic_types::config::GatewayConfig,
) -> u64 {
    config
        .workflow_task_heartbeat_secs
        .unwrap_or_else(|| config.background_tick_secs.clamp(1, 5))
        .clamp(1, 30)
}

fn task_claim_heartbeat_interval_secs(config: &autonoetic_types::config::GatewayConfig) -> u64 {
    workflow_task_heartbeat_interval_secs(config)
}

fn task_claim_stale_after_secs(config: &autonoetic_types::config::GatewayConfig) -> u64 {
    task_claim_heartbeat_interval_secs(config)
        .saturating_mul(4)
        .max(30)
}

/// Process queued workflow tasks: dequeue, create TaskRun records, and spawn child agents.
///
async fn resume_answered_standalone_interactions(
    execution: Arc<crate::execution::GatewayExecutionService>,
) -> anyhow::Result<()> {
    let store = execution
        .gateway_store()
        .ok_or_else(|| anyhow::anyhow!("GatewayStore required"))?;

    let answered = store.get_answered_standalone_interactions()?;
    if answered.is_empty() {
        return Ok(());
    }

    for interaction in answered {
        if !store.try_claim_answered_standalone_interaction_resume(&interaction.interaction_id)? {
            tracing::debug!(
                target: "scheduler",
                interaction_id = %interaction.interaction_id,
                "Standalone interaction already claimed or resumed"
            );
            continue;
        }

        tracing::info!(
            target: "scheduler",
            interaction_id = %interaction.interaction_id,
            session_id = %interaction.session_id,
            agent_id = %interaction.agent_id,
            "Scheduling standalone session resume after answered user interaction"
        );

        let exec = execution.clone();
        let store = store.clone();
        let interaction_id = interaction.interaction_id.clone();
        tokio::spawn(async move {
            let result = exec
                .resume_from_user_interaction(
                    &interaction_id,
                    Some("[scheduler] User answered the pending question; resuming from gateway tick."),
                )
                .await;

            match result {
                Ok(_) => {
                    tracing::info!(
                        target: "scheduler",
                        interaction_id = %interaction_id,
                        "Standalone session resumed successfully"
                    );
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.starts_with("session_waiting_for_approval:") {
                        // Session moved to an ApprovalRequired checkpoint during the resume turn.
                        // The approval path owns the next resume; stop retrying this interaction.
                        tracing::debug!(
                            target: "scheduler",
                            interaction_id = %interaction_id,
                            "Standalone interaction deferred: session is now waiting for approval"
                        );
                    } else {
                        if let Err(release_err) =
                            store.release_answered_standalone_interaction_resume_claim(&interaction_id)
                        {
                            tracing::warn!(
                                target: "scheduler",
                                interaction_id = %interaction_id,
                                error = %release_err,
                                "Failed to release standalone interaction resume claim"
                            );
                        }
                        tracing::warn!(
                            target: "scheduler",
                            interaction_id = %interaction_id,
                            error = %e,
                            "Failed to resume standalone session"
                        );
                    }
                }
            }
        });
    }

    Ok(())
}

/// Called by the scheduler tick after processing background agents.
/// Each queued task must first acquire a durable claim before it is launched.
/// The tokio task runs `spawn_agent_once`, heartbeats its claim, and cleans up on completion.
pub async fn process_queued_workflow_tasks(
    execution: Arc<crate::execution::GatewayExecutionService>,
) -> anyhow::Result<()> {
    let config = execution.config();
    let store = execution.gateway_store();
    let store = store.as_deref();
    let queued = workflow_store::load_all_queued_tasks(&config, store)?;
    if queued.is_empty() {
        return Ok(());
    }

    tracing::info!(
        target: "workflow",
        count = queued.len(),
        "Processing queued workflow tasks"
    );

    for queued_task in queued {
        let existing_task = workflow_store::load_task_run(
            &config,
            store,
            &queued_task.workflow_id,
            &queued_task.task_id,
        )?;

        if let Some(existing) = existing_task.as_ref() {
            match existing.status {
                autonoetic_types::workflow::TaskRunStatus::Succeeded
                | autonoetic_types::workflow::TaskRunStatus::Failed
                | autonoetic_types::workflow::TaskRunStatus::Cancelled
                | autonoetic_types::workflow::TaskRunStatus::Aborted => {
                    let _ = workflow_store::dequeue_task(
                        &config,
                        store,
                        &queued_task.workflow_id,
                        &queued_task.task_id,
                    );
                    continue;
                }
                autonoetic_types::workflow::TaskRunStatus::Running => {
                    tracing::info!(
                        target: "workflow",
                        task_id = %queued_task.task_id,
                        "Task marked Running; claim freshness decides recovery"
                    );
                }
                autonoetic_types::workflow::TaskRunStatus::Paused => {
                    tracing::info!(
                        target: "workflow",
                        task_id = %queued_task.task_id,
                        "Skipping queued launch for paused task"
                    );
                    let _ = workflow_store::dequeue_task(
                        &config,
                        store,
                        &queued_task.workflow_id,
                        &queued_task.task_id,
                    );
                    continue;
                }
                _ => {}
            }
        }

        let Some(_claim) = workflow_store::acquire_task_claim(
            &config,
            store,
            &queued_task.workflow_id,
            &queued_task.task_id,
            task_claim_stale_after_secs(&config),
        )?
        else {
            tracing::debug!(
                target: "workflow",
                task_id = %queued_task.task_id,
                "Task already claimed by a live executor"
            );
            continue;
        };

        if let Some(mut existing) = existing_task {
            if existing.status == autonoetic_types::workflow::TaskRunStatus::Running {
                tracing::info!(
                    target: "workflow",
                    task_id = %queued_task.task_id,
                    "Recovered stale claimed task; re-spawning execution"
                );
            } else {
                existing.status = autonoetic_types::workflow::TaskRunStatus::Running;
                existing.updated_at = chrono::Utc::now().to_rfc3339();
                existing.message = Some(queued_task.message.clone());
                existing.metadata = queued_task.metadata.clone();
                if let Err(e) = workflow_store::save_task_run(&config, store, &existing) {
                    tracing::warn!(
                        target: "workflow",
                        task_id = %queued_task.task_id,
                        error = %e,
                        "Failed to persist claimed task"
                    );
                    let _ = workflow_store::release_task_claim(
                        &config,
                        store,
                        &queued_task.workflow_id,
                        &queued_task.task_id,
                    );
                    continue;
                }

                let _ = workflow_store::append_workflow_event(
                    &config,
                    store,
                    &autonoetic_types::workflow::WorkflowEventRecord {
                        event_id: format!("wevt-{}", &uuid::Uuid::new_v4().to_string()[..8]),
                        workflow_id: queued_task.workflow_id.clone(),
                        task_id: Some(queued_task.task_id.clone()),
                        event_type: "task.started".to_string(),
                        agent_id: Some(queued_task.agent_id.clone()),
                        payload: serde_json::json!({
                            "agent_id": queued_task.agent_id,
                            "child_session_id": queued_task.child_session_id,
                        }),
                        occurred_at: chrono::Utc::now().to_rfc3339(),
                    },
                );
            }
        } else {
            let ts = chrono::Utc::now().to_rfc3339();
            let task_run = autonoetic_types::workflow::TaskRun {
                task_id: queued_task.task_id.clone(),
                workflow_id: queued_task.workflow_id.clone(),
                agent_id: queued_task.agent_id.clone(),
                session_id: queued_task.child_session_id.clone(),
                parent_session_id: queued_task.parent_session_id.clone(),
                status: autonoetic_types::workflow::TaskRunStatus::Running,
                created_at: ts.clone(),
                updated_at: ts,
                source_agent_id: Some(queued_task.source_agent_id.clone()),
                result_summary: None,
                join_group: queued_task.join_group.clone(),
                message: Some(queued_task.message.clone()),
                metadata: queued_task.metadata.clone(),
            };
            if let Err(e) = workflow_store::save_task_run(&config, store, &task_run) {
                tracing::warn!(
                    target: "workflow",
                    task_id = %queued_task.task_id,
                    error = %e,
                    "Failed to save task run"
                );
                let _ = workflow_store::release_task_claim(
                    &config,
                    store,
                    &queued_task.workflow_id,
                    &queued_task.task_id,
                );
                continue;
            }

            let _ = workflow_store::append_workflow_event(
                &config,
                store,
                &autonoetic_types::workflow::WorkflowEventRecord {
                    event_id: format!("wevt-{}", &uuid::Uuid::new_v4().to_string()[..8]),
                    workflow_id: queued_task.workflow_id.clone(),
                    task_id: Some(queued_task.task_id.clone()),
                    event_type: "task.started".to_string(),
                    agent_id: Some(queued_task.agent_id.clone()),
                    payload: serde_json::json!({
                        "agent_id": queued_task.agent_id,
                        "child_session_id": queued_task.child_session_id,
                    }),
                    occurred_at: chrono::Utc::now().to_rfc3339(),
                },
            );
        }

        // Spawn background execution
        let exec = execution.clone();
        let reg = execution.active_executions();
        let agent_id = queued_task.agent_id.clone();
        let message = queued_task.message.clone();
        let session_id = queued_task.child_session_id.clone();
        let source_id = queued_task.source_agent_id.clone();
        let metadata = queued_task.metadata.clone();
        let wf_id = queued_task.workflow_id.clone();
        let t_id = queued_task.task_id.clone();
        let cfg = config.clone();

        let join = tokio::spawn({
            let reg = reg.clone();
            let wf_for_reg = wf_id.clone();
            let tid_for_reg = t_id.clone();
            async move {
                struct Unreg {
                    reg: Arc<crate::runtime::active_execution_registry::ActiveExecutionRegistry>,
                    wf: String,
                    tid: String,
                }
                impl Drop for Unreg {
                    fn drop(&mut self) {
                        self.reg.unregister_workflow_task(&self.wf, &self.tid);
                    }
                }
                let _unreg = Unreg {
                    reg,
                    wf: wf_for_reg,
                    tid: tid_for_reg,
                };
                spawn_task_execution(
                    exec, cfg, wf_id, t_id, agent_id, message, session_id, source_id, metadata,
                )
                .await;
            }
        });
        reg.register_workflow_task(
            &queued_task.workflow_id,
            &queued_task.task_id,
            join.abort_handle(),
        );
    }

    Ok(())
}

/// Execute a task: spawn agent, update status, checkpoint, dequeue on completion.
/// Shared by normal execution, crash recovery, and approval-resume paths.
async fn spawn_task_execution(
    exec: Arc<crate::execution::GatewayExecutionService>,
    cfg: Arc<autonoetic_types::config::GatewayConfig>,
    wf_id: String,
    t_id: String,
    agent_id: String,
    message: String,
    session_id: String,
    source_id: String,
    metadata: Option<serde_json::Value>,
) {
    let store = exec.gateway_store();
    let store = store.as_deref();
    // Load previous task checkpoint (if any) for resume context
    let prev_checkpoint = workflow_store::load_task_checkpoint(&cfg, store, &wf_id, &t_id)
        .ok()
        .flatten();
    let checkpoint_context = if let Some(ref prev) = prev_checkpoint {
        tracing::info!(
            target: "workflow",
            task_id = %t_id,
            prev_step = %prev.step,
            prev_version = prev.version,
            "Resuming task from checkpoint"
        );
        serde_json::json!({
            "agent_id": agent_id,
            "session_id": session_id,
            "message_preview": &message[..message.len().min(120)],
            "resuming_from": { "step": prev.step, "version": prev.version, "state": prev.state },
        })
    } else {
        serde_json::json!({
            "agent_id": agent_id,
            "session_id": session_id,
            "message_preview": &message[..message.len().min(120)],
        })
    };
    let _ = workflow_store::checkpoint_task(
        &cfg,
        store,
        &wf_id,
        &t_id,
        "starting".to_string(),
        checkpoint_context,
    );

    let execution_id = format!(
        "exec-wf-{}-{}",
        t_id,
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    let workflow_run = match workflow_store::load_workflow_run(&cfg, store, &wf_id) {
        Ok(Some(r)) => r,
        Ok(None) => {
            tracing::error!(
                target: "workflow",
                workflow_id = %wf_id,
                task_id = %t_id,
                "missing workflow run for async task"
            );
            return;
        }
        Err(e) => {
            tracing::error!(
                target: "workflow",
                workflow_id = %wf_id,
                error = %e,
                "load_workflow_run failed for async task"
            );
            return;
        }
    };

    let finish_active_row = {
        let gs = exec.gateway_store();
        let eid = execution_id.clone();
        move |status: &str| {
            if let Some(ref g) = gs {
                if let Err(e) = g.complete_active_execution(&eid, status, None) {
                    tracing::debug!(
                        target: "workflow",
                        execution_id = %eid,
                        error = %e,
                        "complete_active_execution"
                    );
                }
            }
        }
    };

    if let Some(gs) = exec.gateway_store() {
        let now = chrono::Utc::now().to_rfc3339();
        let row = gateway_store::ActiveExecutionRecord {
            execution_id: execution_id.clone(),
            root_session_id: workflow_run.root_session_id.clone(),
            workflow_id: Some(wf_id.clone()),
            task_id: Some(t_id.clone()),
            session_id: session_id.clone(),
            agent_id: agent_id.clone(),
            execution_kind: "workflow_task".to_string(),
            driver: None,
            pid: None,
            host_id: gateway_store::default_gateway_host_id(),
            status: "running".to_string(),
            started_at: now.clone(),
            heartbeat_at: now,
            stop_requested_at: None,
            stopped_at: None,
            stop_id: None,
        };
        if let Err(e) = gs.upsert_active_execution(&row) {
            tracing::warn!(
                target: "workflow",
                task_id = %t_id,
                error = %e,
                "upsert_active_execution"
            );
        }
    }

    let heartbeat_cfg = cfg.clone();
    let heartbeat_wf_id = wf_id.clone();
    let heartbeat_task_id = t_id.clone();
    let heartbeat_exec_id = execution_id.clone();
    let heartbeat_gs = exec.gateway_store();
    let heartbeat_interval_secs = task_claim_heartbeat_interval_secs(cfg.as_ref());
    let heartbeat = tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(heartbeat_interval_secs));
        loop {
            interval.tick().await;
            let _ = workflow_store::refresh_task_claim_heartbeat(
                &heartbeat_cfg,
                None,
                &heartbeat_wf_id,
                &heartbeat_task_id,
            );
            let _ = workflow_store::refresh_task_run_heartbeat(
                &heartbeat_cfg,
                None,
                &heartbeat_wf_id,
                &heartbeat_task_id,
            );
            if let Some(ref g) = heartbeat_gs {
                let _ = g.touch_active_execution_heartbeat(&heartbeat_exec_id);
            }
        }
    });

    let result = exec
        .spawn_agent_once(
            &agent_id,
            &message,
            &session_id,
            Some(&source_id),
            false,
            None,
            metadata.as_ref(),
            Some(&wf_id),
            Some(&t_id),
            None,
        )
        .await;

    heartbeat.abort();
    let _ = heartbeat.await;

    match result {
        Ok(spawn_result) => {
            // Check if the turn was suspended at an approval gate (continuation already on disk).
            if let Some(ref request_id) = spawn_result.suspended_for_approval {
                let summary = format!("awaiting approval {}", request_id);

                // Load continuation to get approval details (tool name, etc.)
                let approval_metadata = continuation::load_continuation(&cfg, &t_id)
                    .ok()
                    .flatten()
                    .and_then(|cont| {
                        let tool_name = cont.pending_tool_call.tool_name.clone();
                        // Derive approval kind from tool name
                        let kind = if tool_name.contains("sandbox") {
                            "sandbox".to_string()
                        } else if tool_name.contains("install") {
                            "agent_install".to_string()
                        } else {
                            "tool_execution".to_string()
                        };
                        // Try to extract reason from approval_response
                        let reason = serde_json::from_str::<serde_json::Value>(
                            &cont.pending_tool_call.approval_response,
                        )
                        .ok()
                        .and_then(|v| {
                            v.get("approval")
                                .and_then(|a| a.get("reason"))
                                .and_then(|r| r.as_str())
                                .map(String::from)
                        });

                        Some(workflow_store::ApprovalMetadata {
                            request_id: cont.approval_request_id,
                            kind,
                            reason,
                        })
                    });

                if let Err(e) = workflow_store::update_task_run_status(
                    &cfg,
                    store,
                    &wf_id,
                    &t_id,
                    autonoetic_types::workflow::TaskRunStatus::AwaitingApproval,
                    Some(summary),
                    approval_metadata,
                    None,
                ) {
                    tracing::warn!(
                        target: "workflow",
                        error = %e,
                        task_id = %t_id,
                        "Failed to persist async task awaiting approval status"
                    );
                }
                let _ = workflow_store::checkpoint_task(
                    &cfg,
                    store,
                    &wf_id,
                    &t_id,
                    "awaiting_approval".to_string(),
                    serde_json::json!({
                        "status": "awaiting_approval",
                        "approval_request_id": request_id,
                    }),
                );
                let _ = workflow_store::dequeue_task(&cfg, store, &wf_id, &t_id);
                tracing::info!(
                    target: "workflow",
                    task_id = %t_id,
                    approval_request_id = %request_id,
                    "Turn suspended at approval gate; continuation saved; task awaiting approval"
                );
                finish_active_row("stopped");
                return;
            }

            // A child can also suspend on user input or human escalation.
            // Keep the task non-terminal so workflow join does not fire early.
            if spawn_result.suspended_for_user_input {
                let pending_for_session =
                    crate::scheduler::approval::pending_approval_requests_for_session(
                        &cfg,
                        store,
                        &session_id,
                    )
                    .unwrap_or_default();

                if let Some(request) = pending_for_session.first() {
                    let summary = format!("awaiting approval {}", request.request_id);
                    let approval_metadata = Some(workflow_store::ApprovalMetadata {
                        request_id: request.request_id.clone(),
                        kind: request.action.kind().to_string(),
                        reason: request.reason.clone(),
                    });
                    if let Err(e) = workflow_store::update_task_run_status(
                        &cfg,
                        store,
                        &wf_id,
                        &t_id,
                        autonoetic_types::workflow::TaskRunStatus::AwaitingApproval,
                        Some(summary),
                        approval_metadata,
                        None,
                    ) {
                        tracing::warn!(
                            target: "workflow",
                            error = %e,
                            task_id = %t_id,
                            "Failed to persist async task awaiting approval status"
                        );
                    }
                    let _ = workflow_store::checkpoint_task(
                        &cfg,
                        store,
                        &wf_id,
                        &t_id,
                        "awaiting_approval".to_string(),
                        serde_json::json!({
                            "status": "awaiting_approval",
                            "approval_request_id": request.request_id,
                        }),
                    );
                    let _ = workflow_store::dequeue_task(&cfg, store, &wf_id, &t_id);
                    tracing::info!(
                        target: "workflow",
                        task_id = %t_id,
                        approval_request_id = %request.request_id,
                        "Task suspended with pending approval after non-terminal yield"
                    );
                    finish_active_row("stopped");
                    return;
                }

                let summary = Some("paused: awaiting user input".to_string());
                if let Err(e) = workflow_store::update_task_run_status(
                    &cfg,
                    store,
                    &wf_id,
                    &t_id,
                    autonoetic_types::workflow::TaskRunStatus::Paused,
                    summary,
                    None,
                    None,
                ) {
                    tracing::warn!(
                        target: "workflow",
                        error = %e,
                        task_id = %t_id,
                        "Failed to persist paused workflow task status"
                    );
                }
                let _ = workflow_store::checkpoint_task(
                    &cfg,
                    store,
                    &wf_id,
                    &t_id,
                    "paused".to_string(),
                    serde_json::json!({
                        "status": "paused",
                        "reason": "awaiting_user_input_or_operator_guidance",
                    }),
                );
                let _ = workflow_store::dequeue_task(&cfg, store, &wf_id, &t_id);
                tracing::info!(
                    target: "workflow",
                    task_id = %t_id,
                    "Task paused after non-terminal yield"
                );
                finish_active_row("stopped");
                return;
            }

            let summary = spawn_result.assistant_reply.as_ref().map(|s| {
                const MAX: usize = 512;
                if s.len() <= MAX {
                    s.clone()
                } else {
                    format!("{}…", &s[..MAX])
                }
            });
            if let Err(e) = workflow_store::update_task_run_status(
                &cfg,
                store,
                &wf_id,
                &t_id,
                autonoetic_types::workflow::TaskRunStatus::Succeeded,
                summary,
                None,
                None,
            ) {
                tracing::warn!(target: "workflow", error = %e, "Failed to persist async task completion");
            }
            tracing::info!(target: "workflow", task_id = %t_id, "Async task completed successfully");
            let _ = workflow_store::checkpoint_task(
                &cfg,
                store,
                &wf_id,
                &t_id,
                "completed".to_string(),
                serde_json::json!({
                    "status": "succeeded",
                    "result_summary": spawn_result.assistant_reply.as_ref().map(|s| &s[..s.len().min(200)]),
                }),
            );
            let _ = workflow_store::dequeue_task(&cfg, store, &wf_id, &t_id);
            finish_active_row("stopped");
        }
        Err(e) => {
            if let Err(inner) = workflow_store::update_task_run_status(
                &cfg,
                store,
                &wf_id,
                &t_id,
                autonoetic_types::workflow::TaskRunStatus::Failed,
                Some(e.to_string()),
                None,
                None,
            ) {
                tracing::warn!(target: "workflow", error = %inner, "Failed to persist async task failure");
            }
            tracing::warn!(target: "workflow", task_id = %t_id, error = %e, "Async task failed");
            let _ = workflow_store::checkpoint_task(
                &cfg,
                store,
                &wf_id,
                &t_id,
                "failed".to_string(),
                serde_json::json!({ "status": "failed", "error": e.to_string() }),
            );
            // Append validation errors to digest for better visibility
            let error_str = e.to_string();
            let is_validation_error = error_str.contains("validation failed")
                || error_str.contains("response_validation")
                || error_str.contains("artifact_build_evidence")
                || error_str.contains("repair");
            if is_validation_error {
                use crate::runtime::live_digest::base_session_id;
                let base_session = base_session_id(&session_id);
                let is_repair = error_str.contains("repair") || error_str.contains("deadline");
                crate::runtime::live_digest::append_validation_error_best_effort(
                    &cfg.agents_dir,
                    &base_session,
                    &error_str,
                    is_repair,
                );
            }
            let _ = workflow_store::dequeue_task(&cfg, store, &wf_id, &t_id);
            finish_active_row("stopped");
        }
    }
}

/// Scan all workflows for Runnable tasks (approval-unblocked) and execute them.
///
/// When a task is unblocked by approval resolution (AwaitingApproval → Runnable),
/// it is re-queued into the same durable execution path used by async spawns.
pub async fn process_runnable_workflow_tasks(
    execution: Arc<crate::execution::GatewayExecutionService>,
) -> anyhow::Result<()> {
    let config = execution.config();
    let workflows_root = workflow_store::workflows_root(&config).join("runs");
    if !workflows_root.is_dir() {
        return Ok(());
    }

    for entry in std::fs::read_dir(&workflows_root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let wf_id = entry.file_name().to_string_lossy().to_string();
        let store = execution.gateway_store();
        let store = store.as_deref();
        let workflow_run = workflow_store::load_workflow_run(&config, store, &wf_id)?;
        if let Some(ref wr) = workflow_run {
            if matches!(
                wr.status,
                autonoetic_types::workflow::WorkflowRunStatus::EmergencyStopping
                    | autonoetic_types::workflow::WorkflowRunStatus::EmergencyStopped
            ) {
                continue;
            }
        }
        let tasks = workflow_store::list_task_runs_for_workflow(&config, store, &wf_id)?;

        for task in tasks {
            if task.status != autonoetic_types::workflow::TaskRunStatus::Runnable {
                continue;
            }

            let blocks_planner = workflow_run
                .as_ref()
                .is_some_and(|run| run.join_task_ids.contains(&task.task_id));

            tracing::info!(
                target: "workflow",
                workflow_id = %wf_id,
                task_id = %task.task_id,
                agent_id = %task.agent_id,
                "Re-queueing approval-unblocked task for durable execution"
            );

            if workflow_store::queued_task_exists(&config, &wf_id, &task.task_id) {
                if let Err(e) = workflow_store::refresh_queued_task_message_from_task_checkpoint(
                    &config,
                    store,
                    &wf_id,
                    &task.task_id,
                ) {
                    tracing::warn!(
                        target: "workflow",
                        error = %e,
                        workflow_id = %wf_id,
                        task_id = %task.task_id,
                        "Failed to refresh queued task from checkpoint"
                    );
                }
                continue;
            }

            let queued = autonoetic_types::workflow::QueuedTaskRun {
                task_id: task.task_id.clone(),
                workflow_id: wf_id.clone(),
                agent_id: task.agent_id.clone(),
                message: task
                    .message
                    .clone()
                    .unwrap_or_else(|| format!("Resume after approval: {}", task.session_id)),
                child_session_id: task.session_id.clone(),
                parent_session_id: task.parent_session_id.clone(),
                source_agent_id: task.source_agent_id.clone().unwrap_or_default(),
                metadata: task.metadata.clone(),
                join_group: task.join_group.clone(),
                blocks_planner,
                enqueued_at: chrono::Utc::now().to_rfc3339(),
            };
            workflow_store::enqueue_task(&config, store, &queued)?;
        }
    }

    Ok(())
}

pub fn append_task_board_entry(
    config: &autonoetic_types::config::GatewayConfig,
    entry: &autonoetic_types::task_board::TaskBoardEntry,
) -> anyhow::Result<()> {
    store::append_jsonl_record(&store::task_board_path(config), entry)
}

async fn process_pending_notifications(
    execution: Arc<crate::execution::GatewayExecutionService>,
    store: &crate::scheduler::gateway_store::GatewayStore,
) -> anyhow::Result<()> {
    let pending = store.list_pending_notifications()?;
    if pending.is_empty() {
        return Ok(());
    }

    let port = execution.config().port;
    let timeout_secs = execution.config().signal_delivery_timeout_secs;

    for n in pending {
        // Map NotificationRecord payload to Signal.
        // Only current Signal payloads are accepted for deterministic delivery.
        let signal = match n.notification_type {
            autonoetic_types::notification::NotificationType::ApprovalResolved => {
                serde_json::from_value::<crate::scheduler::signal::Signal>(n.payload.clone()).ok()
            }
            autonoetic_types::notification::NotificationType::WorkflowJoinSatisfied => {
                serde_json::from_value::<crate::scheduler::signal::Signal>(n.payload.clone()).ok()
            }
            autonoetic_types::notification::NotificationType::AgentMessage => {
                serde_json::from_value::<crate::scheduler::signal::Signal>(n.payload.clone()).ok()
            }
            autonoetic_types::notification::NotificationType::AdminProposal => None,
            autonoetic_types::notification::NotificationType::ConstitutionalProposal => None,
        };

        if let Some(signal) = signal {
            let pending_signal = crate::scheduler::signal::PendingSignal {
                request_id: n
                    .request_id
                    .clone()
                    .unwrap_or_else(|| n.notification_id.clone()),
                signal,
                filename: format!(
                    "{}.json",
                    n.request_id.as_deref().unwrap_or(&n.notification_id)
                ),
            };
            if let Err(e) = crate::scheduler::signal::deliver_signal(
                &pending_signal,
                &n.target_session_id,
                port,
                timeout_secs,
            )
            .await
            {
                tracing::warn!(notification_id = %n.notification_id, error = %e, "Failed to deliver signal");

                // Track attempts and eventually fail so malformed/unreachable
                // notifications don't stay pending forever.
                let _ = store.increment_attempt(&n.notification_id, Some(&e.to_string()));
                let next_attempt = n.attempt_count.saturating_add(1);
                if next_attempt >= 3 {
                    let _ = store.update_notification_status(
                        &n.notification_id,
                        autonoetic_types::notification::NotificationStatus::Failed,
                    );
                }
            } else {
                store.update_notification_status(
                    &n.notification_id,
                    autonoetic_types::notification::NotificationStatus::Delivered,
                )?;
            }
        } else {
            // Payload is not interpretable as a supported notification signal:
            // fail deterministically instead of leaving it pending forever.
            let _ = store.increment_attempt(
                &n.notification_id,
                Some("Unrecognized notification payload for scheduler delivery"),
            );
            let _ = store.update_notification_status(
                &n.notification_id,
                autonoetic_types::notification::NotificationStatus::Failed,
            );
        }
    }
    Ok(())
}

async fn process_due_scheduled_jobs(
    execution: Arc<crate::execution::GatewayExecutionService>,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<()> {
    let config = execution.config();
    let max_due = config.scheduled_jobs.max_due_per_tick;
    let now_rfc = now.to_rfc3339();

    let store = match execution.gateway_store() {
        Some(s) => s,
        None => return Ok(()),
    };

    let due_jobs = store.load_due_scheduled_jobs(&now_rfc, max_due)?;
    if due_jobs.is_empty() {
        return Ok(());
    }

    for job in due_jobs {
        let cron = match crate::scheduler::cron_parser::parse_schedule(&job.cron_expr) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    target: "scheduler",
                    job_id = %job.job_id,
                    cron_expr = %job.cron_expr,
                    error = %e,
                    "Failed to parse cron expression; cancelling job"
                );
                let _ = store.cancel_scheduled_job(&job.job_id);
                continue;
            }
        };

        let next_occurrence = crate::scheduler::cron_parser::next_occurrence(&cron, now);
        let next_run_at = match next_occurrence {
            Some(n) => n.to_rfc3339(),
            None => {
                tracing::warn!(
                    target: "scheduler",
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
                tracing::debug!(
                    target: "scheduler",
                    job_id = %job.job_id,
                    "Could not claim scheduled job (already claimed or not due)"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    target: "scheduler",
                    job_id = %job.job_id,
                    error = %e,
                    "Failed to claim scheduled job"
                );
                continue;
            }
        };

        tracing::info!(
            target: "scheduler",
            job_id = %claimed.job_id,
            owner_agent_id = %claimed.owner_agent_id,
            target_agent_id = %claimed.target_agent_id,
            "Triggering scheduled job"
        );

        let workflow_id = format!("sched-{}", &claimed.job_id);
        let task_id = format!(
            "task-{}-{}",
            &claimed.job_id,
            &uuid::Uuid::new_v4().to_string()[..8]
        );

        let mut run =
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
                            target: "scheduler",
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
                        target: "scheduler",
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
            })),
            join_group: None,
            blocks_planner: false,
            enqueued_at: now_rfc.clone(),
        };

        if let Err(e) = workflow_store::enqueue_task(&config, Some(store.as_ref()), &queued) {
            tracing::warn!(
                target: "scheduler",
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

        workflow_store::save_workflow_run(&config, Some(store.as_ref()), &run)?;

        let trigger_event = autonoetic_types::workflow::WorkflowEventRecord {
            event_id: format!("wevt-sched-{}", &task_id),
            workflow_id: workflow_id.clone(),
            event_type: "scheduled_job.triggered".to_string(),
            task_id: Some(task_id.clone()),
            agent_id: Some(claimed.target_agent_id.clone()),
            payload: serde_json::json!({
                "job_id": claimed.job_id,
                "owner_agent_id": claimed.owner_agent_id,
                "scheduled_for": next_run_at,
            }),
            occurred_at: now_rfc.clone(),
        };
        let _ =
            workflow_store::append_workflow_event(&config, Some(store.as_ref()), &trigger_event);
    }

    Ok(())
}
