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

use autonoetic_types::causal_chain::CausalEventRecord;

pub mod agent_outcome;
pub mod approval;
pub mod approval_hardening;
pub mod auto_learning_jobs;
pub mod cron_parser;
pub mod decision;
pub mod eval_runner;
pub mod fast_scheduler;
pub mod gateway_store;
pub mod hooks;
pub mod overflow_classifier;
pub mod plan_frame_ops;
pub mod reclamation;
pub mod runner;
pub mod session_envelope_ops;
pub mod signal;
pub mod single_flight;
pub mod store;
pub mod system_agents;
pub mod task_notify;
pub mod workflow_causal;
pub mod workflow_store;

pub use approval::*;
pub use decision::*;
pub use gateway_store::*;
pub use plan_frame_ops::*;
pub use runner::*;
pub use session_envelope_ops::*;
pub use signal::*;
pub use single_flight::*;
pub use store::*;
pub use workflow_causal::*;
pub use workflow_store::*;

pub async fn start_background_scheduler(
    router: Arc<crate::router::JsonRpcRouter>,
) -> anyhow::Result<()> {
    let execution = router.execution_service();
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
        if let Err(e) = run_scheduler_tick_with_router(router.clone()).await {
            tracing::warn!(error = %e, "Background scheduler tick failed");
        }
    }
}

pub async fn run_scheduler_tick_with_router(
    router: Arc<crate::router::JsonRpcRouter>,
) -> anyhow::Result<()> {
    run_scheduler_tick_common(router.execution_service(), Some(router), Utc::now()).await
}

pub async fn run_scheduler_tick(
    execution: Arc<crate::execution::GatewayExecutionService>,
) -> anyhow::Result<()> {
    run_scheduler_tick_common(execution, None, Utc::now()).await
}

async fn run_scheduler_tick_common(
    execution: Arc<crate::execution::GatewayExecutionService>,
    router: Option<Arc<crate::router::JsonRpcRouter>>,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    // Process pending notifications
    if let Some(store) = execution.gateway_store() {
        if let Err(e) = process_pending_notifications(execution.clone(), store.as_ref(), router.clone()).await {
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
        if degrade_threshold > 0
            && (emergency_threshold == 0 || degrade_threshold < emergency_threshold)
        {
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
    if let Some(store) = execution.gateway_store() {
        if let Err(e) = crate::scheduler::single_flight::cleanup_stale_reservations(
            &config,
            Some(store.as_ref()),
        ) {
            tracing::warn!(error = %e, "Failed to clean up stale single-flight reservations");
        }
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

    // Mark standalone (non-workflow) approvals and user interactions whose TTL
    // has passed as stale/expired. They remain resolvable for the operator.
    if let Some(store) = execution.gateway_store() {
        if let Err(e) = store.flag_expired_standalone_approvals() {
            tracing::warn!(error = %e, "Failed to flag expired standalone approvals");
        }
        if let Err(e) = store.expire_timed_out_interactions() {
            tracing::warn!(error = %e, "Failed to expire timed-out interactions");
        }
        if let Err(e) = store.expire_timed_out_escalations() {
            tracing::warn!(error = %e, "Failed to expire timed-out escalations");
        }
        if let Err(e) = store.expire_timed_out_plan_frames() {
            tracing::warn!(error = %e, "Failed to expire timed-out plan frames");
        }
    }

    // Wiki proposal auto-expiry: cancel proposals older than configured TTL.
    if let Err(e) = check_wiki_proposal_expiry(execution.clone()).await {
        tracing::warn!(error = %e, "Failed to check wiki proposal expiry");
    }

    // Adjudication SLA (#771 D.1): flag constitutional proposals (O-6) and
    // anomaly flags (O-7) that have sat un-adjudicated past the deadline.
    if let Err(e) = check_adjudication_sla_breaches(execution.clone()).await {
        tracing::warn!(error = %e, "Failed to check adjudication SLA breaches");
    }

    // Amendment invitations (#771 D.2): repeated denials of the same rule
    // for the same alias become a durable invitation to draft an amendment.
    if let Err(e) = check_amendment_invitation_thresholds(execution.clone()).await {
        tracing::warn!(error = %e, "Failed to check amendment invitation thresholds");
    }

    // Detect and resolve tasks stuck in Running state (child session completed but task status not updated).
    if let Err(e) = check_stuck_running_tasks(execution.clone()).await {
        tracing::warn!(error = %e, "Failed to check stuck running tasks");
    }

    // Janitor: re-evaluate Paused child-wait tasks and repair active_task_ids
    // drift, so a missed wake can never deadlock a workflow permanently.
    if let Err(e) = reconcile_paused_child_wait_tasks(execution.clone()).await {
        tracing::warn!(error = %e, "Failed to reconcile paused child-wait tasks");
    }

    // Orphan-child reaper: cancel children of terminated parent sessions (R+12)
    if let Err(e) = reap_orphaned_sessions(execution.clone()).await {
        tracing::warn!(error = %e, "Failed to reap orphaned sessions");
    }

    // Resource reclamation sweep: garbage collect content blobs, old revisions,
    // expired memories, orphaned sessions, and stale scheduled jobs.
    if let Some(store) = execution.gateway_store() {
        let reclamation_cfg = &config.reclamation;
        if reclamation_cfg.enabled {
            let gateway_dir = crate::execution::gateway_root_dir(&config);
            let now = Utc::now();
            if let Err(e) = crate::scheduler::reclamation::run_reclamation_sweep(
                &gateway_dir,
                store.as_ref(),
                reclamation_cfg,
                &now,
            ) {
                tracing::warn!(error = %e, "Resource reclamation sweep failed");
            }
        }
    }

    // Post-promotion background review (Phase 4, Tier 1).
    // Runs daily per-agent: checks causal event trends, sentinel findings.
    if let Some(store) = execution.gateway_store() {
        if let Err(e) = crate::post_promotion_review::run_post_promotion_review(&store) {
            tracing::warn!(error = %e, "Post-promotion review failed");
        }
    }

    // Resume standalone sessions whose user interaction has been answered
    if let Err(e) = resume_answered_standalone_interactions(execution).await {
        tracing::warn!(error = %e, "Failed to resume answered standalone interactions");
    }

    Ok(())
}

async fn check_wiki_proposal_expiry(
    execution: Arc<crate::execution::GatewayExecutionService>,
) -> anyhow::Result<()> {
    let config = execution.config();
    let ttl_secs = config.wiki_proposal.auto_expire_secs;
    if ttl_secs == 0 {
        return Ok(());
    }
    let store = match execution.gateway_store() {
        Some(s) => s,
        None => return Ok(()),
    };
    let pending = store.get_pending_approvals()?;
    let now = chrono::Utc::now();
    let mut expired = 0;
    for req in &pending {
        if !matches!(
            req.action,
            autonoetic_types::background::ScheduledAction::WikiProposal { .. }
        ) {
            continue;
        }
        let created = match chrono::DateTime::parse_from_rfc3339(&req.created_at) {
            Ok(dt) => dt.with_timezone(&chrono::Utc),
            Err(e) => {
                tracing::warn!(
                    target: "wiki_proposal_expiry",
                    request_id = %req.request_id,
                    created_at = %req.created_at,
                    error = %e,
                    "Skipping wiki proposal with unparseable created_at"
                );
                continue;
            }
        };
        if (now - created).num_seconds() > ttl_secs as i64 {
            match crate::scheduler::approval::cancel_request(
                &config,
                Some(&store),
                &req.request_id,
                "system",
                Some("auto-expired".to_string()),
                None,
            ) {
                Ok(_) => {
                    expired += 1;
                    tracing::info!(
                        target: "wiki_proposal_expiry",
                        request_id = %req.request_id,
                        "Auto-expired wiki proposal"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "wiki_proposal_expiry",
                        request_id = %req.request_id,
                        error = %e,
                        "Failed to cancel expired wiki proposal"
                    );
                }
            }
        }
    }
    if expired > 0 {
        tracing::info!(target: "wiki_proposal_expiry", expired, "Wiki proposal auto-expiry sweep completed");
    }
    Ok(())
}

/// Adjudication SLA (#771 D.1, O-6 / O-7): flag constitutional proposals and
/// anomaly flags that have sat un-adjudicated past `adjudication_sla_secs`.
/// A breach is idempotent (stamped once) and does NOT change the item's
/// status — the decision is still owed.
async fn check_adjudication_sla_breaches(
    execution: Arc<crate::execution::GatewayExecutionService>,
) -> anyhow::Result<()> {
    let config = execution.config();
    if !config.decider_obligations.enabled || config.decider_obligations.adjudication_sla_secs == 0
    {
        return Ok(());
    }
    let sla_secs = config.decider_obligations.adjudication_sla_secs;

    let store = match execution.gateway_store() {
        Some(s) => s,
        None => return Ok(()),
    };

    let now = chrono::Utc::now().to_rfc3339();

    let breached_proposals = store.flag_proposal_sla_breaches(sla_secs, &now)?;
    for proposal in &breached_proposals {
        let age_secs = chrono::DateTime::parse_from_rfc3339(&proposal.created_at)
            .ok()
            .map(|created| (chrono::Utc::now() - created.with_timezone(&chrono::Utc)).num_seconds())
            .unwrap_or(sla_secs as i64);

        // O-6 entered the enforcement register with the 2026.07.19 amendment,
        // so this event attributes to O-6 in contract-health (pre-enactment
        // it bucketed as `unattributed`); the rule ID was carried all along
        // so attribution went live at signing without an event-shape change.
        let event = CausalEventRecord {
            event_id: format!("sla-ev-{}", uuid::Uuid::new_v4()),
            agent_id: proposal.proposer_agent_id.clone(),
            session_id: proposal
                .proposer_session_id
                .clone()
                .unwrap_or_else(|| "system".to_string()),
            turn_id: None,
            event_seq: 0,
            timestamp: now.clone(),
            category: "decider_obligation".to_string(),
            action: "sla_breached".to_string(),
            status: "active".to_string(),
            enforced_rules: vec!["O-6".to_string()],
            target: Some(proposal.proposal_id.clone()),
            payload: Some(
                serde_json::json!({
                    "id": proposal.proposal_id,
                    "kind": "constitutional_proposal",
                    "age_secs": age_secs,
                    "sla_secs": sla_secs,
                })
                .to_string(),
            ),
            payload_ref: None,
            evidence_ref: None,
            reason: Some("adjudication SLA breached — decision still owed".to_string()),
        };
        if let Err(e) = store.create_causal_event(&event) {
            tracing::warn!(
                proposal_id = %proposal.proposal_id,
                error = %e,
                "Failed to emit adjudication SLA breach causal event for constitutional proposal"
            );
        }

        let notification = autonoetic_types::notification::NotificationRecord::new(
            autonoetic_types::id_format::short_random_id("ntf-"),
            autonoetic_types::notification::NotificationType::ConstitutionalProposal,
            // Gateway-detected breach, not tied to a session — mirror the
            // filing-notification pattern ("system"); the owed party lives in
            // the payload (`owed_to`) rather than the session-id field.
            "system".to_string(),
            serde_json::json!({
                "event": "sla_breached",
                "id": proposal.proposal_id,
                "owed_to": proposal.proposer_agent_id,
                "age_secs": age_secs,
                "sla_secs": sla_secs,
            }),
        );
        if let Err(e) = store.create_notification_record(&notification) {
            tracing::warn!(
                proposal_id = %proposal.proposal_id,
                error = %e,
                "Failed to create adjudication SLA breach notification for constitutional proposal"
            );
        }
    }

    let breached_flags = store.flag_anomaly_flag_sla_breaches(sla_secs, &now)?;
    for flag in &breached_flags {
        let age_secs = chrono::DateTime::parse_from_rfc3339(&flag.created_at)
            .ok()
            .map(|created| (chrono::Utc::now() - created.with_timezone(&chrono::Utc)).num_seconds())
            .unwrap_or(sla_secs as i64);

        // O-7 entered the enforcement register with the 2026.07.19 amendment,
        // so this event attributes to O-7 in contract-health (pre-enactment
        // it bucketed as `unattributed`); the rule ID was carried all along
        // so attribution went live at signing without an event-shape change.
        let event = CausalEventRecord {
            event_id: format!("sla-ev-{}", uuid::Uuid::new_v4()),
            agent_id: flag.reporter_agent_id.clone(),
            session_id: flag
                .reporter_session_id
                .clone()
                .unwrap_or_else(|| "system".to_string()),
            turn_id: None,
            event_seq: 0,
            timestamp: now.clone(),
            category: "decider_obligation".to_string(),
            action: "sla_breached".to_string(),
            status: "active".to_string(),
            enforced_rules: vec!["O-7".to_string()],
            target: Some(flag.flag_id.clone()),
            payload: Some(
                serde_json::json!({
                    "id": flag.flag_id,
                    "kind": "anomaly_flag",
                    "age_secs": age_secs,
                    "sla_secs": sla_secs,
                })
                .to_string(),
            ),
            payload_ref: None,
            evidence_ref: None,
            reason: Some("adjudication SLA breached — decision still owed".to_string()),
        };
        if let Err(e) = store.create_causal_event(&event) {
            tracing::warn!(
                flag_id = %flag.flag_id,
                error = %e,
                "Failed to emit adjudication SLA breach causal event for anomaly flag"
            );
        }

        let notification = autonoetic_types::notification::NotificationRecord::new(
            autonoetic_types::id_format::short_random_id("ntf-"),
            autonoetic_types::notification::NotificationType::AnomalyFlag,
            // Gateway-detected breach, not tied to a session — see the
            // proposal branch above; owed party is carried in `owed_to`.
            "system".to_string(),
            serde_json::json!({
                "event": "sla_breached",
                "id": flag.flag_id,
                "owed_to": flag.reporter_agent_id,
                "age_secs": age_secs,
                "sla_secs": sla_secs,
            }),
        );
        if let Err(e) = store.create_notification_record(&notification) {
            tracing::warn!(
                flag_id = %flag.flag_id,
                error = %e,
                "Failed to create adjudication SLA breach notification for anomaly flag"
            );
        }
    }

    Ok(())
}

/// Amendment invitations (#771 D.2, citizenship RFC Part D): when the same
/// rule is denied to the same agent alias at least
/// `amendment_invitations.threshold` times within `window_secs`, issue a
/// durable invitation to draft an amendment (Ri-0.8). The gateway never
/// judges the rule — it executes a pre-committed threshold (Lawful
/// Executor). Issuance is race-safe (partial unique index on OPEN
/// (agent_id, rule_id)); per issuance the tick emits an
/// `amendment_invitation.issued` causal event and a notification so the
/// invitation is visible outside the attestation line too. Open invitations
/// older than their window are expired in the same tick.
async fn check_amendment_invitation_thresholds(
    execution: Arc<crate::execution::GatewayExecutionService>,
) -> anyhow::Result<()> {
    let config = execution.config();
    let inv_cfg = &config.amendment_invitations;
    if !inv_cfg.enabled || inv_cfg.threshold == 0 || inv_cfg.window_secs == 0 {
        return Ok(());
    }
    let store = match execution.gateway_store() {
        Some(s) => s,
        None => return Ok(()),
    };

    let now = chrono::Utc::now().to_rfc3339();

    // Bookkeeping first: invitations whose window fully elapsed leave the
    // attestation line (and free their (agent, rule) pair for re-issue).
    let expired = store.expire_amendment_invitations(&now)?;
    if !expired.is_empty() {
        tracing::info!(
            target: "amendment_invitation",
            expired = expired.len(),
            "Expired stale amendment invitations"
        );
    }

    let tallies = store.denial_tallies_by_rule(inv_cfg.window_secs, &now)?;
    for tally in tallies {
        if tally.count < inv_cfg.threshold {
            continue;
        }
        let invitation = crate::scheduler::gateway_store::amendment_invitations::AmendmentInvitation {
            invitation_id: format!("ainv-{}", uuid::Uuid::new_v4()),
            agent_id: tally.agent_id.clone(),
            rule_id: tally.rule_id.clone(),
            denial_count: tally.count,
            threshold: inv_cfg.threshold,
            window_secs: inv_cfg.window_secs,
            status: "open".to_string(),
            answered_proposal_id: None,
            created_at: now.clone(),
            resolved_at: None,
        };
        // Race-safe dedup: false means an open invitation already exists for
        // this (agent, rule) — no event, no notification, no duplicate row.
        if !store.insert_amendment_invitation(&invitation)? {
            continue;
        }

        tracing::info!(
            target: "amendment_invitation",
            invitation_id = %invitation.invitation_id,
            agent_id = %tally.agent_id,
            rule_id = %tally.rule_id,
            denial_count = tally.count,
            threshold = inv_cfg.threshold,
            window_secs = inv_cfg.window_secs,
            "Issued amendment invitation from denial telemetry"
        );

        // Ri-0.8 (right to propose) IS in the enforcement register, so this
        // event attributes in contract-health immediately — the invitation
        // is the gateway honoring the right's spirit, not a discretion
        // exercise. Payload carries the denial statistics (the friction
        // evidence) so the adjudicating seat sees the pattern's shape, not
        // just its count (#771 open question).
        let event = CausalEventRecord {
            event_id: format!("ainv-ev-{}", uuid::Uuid::new_v4()),
            agent_id: tally.agent_id.clone(),
            session_id: "system".to_string(),
            turn_id: None,
            event_seq: 0,
            timestamp: now.clone(),
            category: "amendment_invitation".to_string(),
            action: "issued".to_string(),
            status: "recorded".to_string(),
            enforced_rules: vec!["Ri-0.8".to_string()],
            target: Some(invitation.invitation_id.clone()),
            payload: Some(
                serde_json::json!({
                    "invitation_id": invitation.invitation_id,
                    "agent_id": tally.agent_id,
                    "rule_id": tally.rule_id,
                    "denial_count": tally.count,
                    "threshold": inv_cfg.threshold,
                    "window_secs": inv_cfg.window_secs,
                })
                .to_string(),
            ),
            payload_ref: None,
            evidence_ref: None,
            reason: Some(
                "repeated denials of the same rule — invited to draft an amendment (Ri-0.8)"
                    .to_string(),
            ),
        };
        if let Err(e) = store.create_causal_event(&event) {
            tracing::warn!(
                invitation_id = %invitation.invitation_id,
                error = %e,
                "Failed to emit amendment invitation causal event"
            );
        }

        let notification = autonoetic_types::notification::NotificationRecord::new(
            autonoetic_types::id_format::short_random_id("ntf-"),
            autonoetic_types::notification::NotificationType::ConstitutionalProposal,
            // Gateway-issued, not tied to a session — mirror the SLA-breach
            // pattern ("system"); the invited party lives in the payload.
            "system".to_string(),
            serde_json::json!({
                "event": "amendment_invitation_issued",
                "invitation_id": invitation.invitation_id,
                "agent_id": tally.agent_id,
                "rule_id": tally.rule_id,
                "denial_count": tally.count,
                "threshold": inv_cfg.threshold,
                "window_secs": inv_cfg.window_secs,
            }),
        );
        if let Err(e) = store.create_notification_record(&notification) {
            tracing::warn!(
                invitation_id = %invitation.invitation_id,
                error = %e,
                "Failed to create amendment invitation notification"
            );
        }
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

            // Read `suspended_at` from the session checkpoint (supersedes continuation).
            let sid = &task.session_id;
            let suspended_at: Option<chrono::DateTime<chrono::Utc>> = {
                let cp = crate::runtime::checkpoint::load_latest_checkpoint(&config, sid)
                    .ok()
                    .flatten();
                cp.and_then(|cp| {
                    let ts = cp.suspended_at?;
                    chrono::DateTime::parse_from_rfc3339(&ts)
                        .ok()
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                })
            };

            let timed_out = match suspended_at {
                // Use signed comparison to avoid u64 wraparound on clock skew
                // or tampered timestamps that produce a negative duration.
                Some(ts) => (now - ts).num_seconds() > timeout_secs as i64,
                None => false,
            };

            if timed_out {
                tracing::warn!(
                    target: "workflow",
                    workflow_id = %wf_id,
                    task_id = %task.task_id,
                    timeout_secs = timeout_secs,
                    "Approval timeout expired; marking task as stale (checkpoint preserved for late approval)"
                );

                let reason = "Approval timed out".to_string();
                // Mark as Stale — the checkpoint is preserved so the operator
                // can still approve later and resume the task (P-2.11, P-7.11).
                let _ = workflow_store::update_task_run_status(
                    &config,
                    store,
                    &wf_id,
                    &task.task_id,
                    autonoetic_types::workflow::TaskRunStatus::Stale,
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
                // the operator approves later (P-2.11, P-7.11). Just emit workflow event.
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
                if let Ok(Some(cp)) =
                    crate::runtime::checkpoint::load_latest_checkpoint(&config, &task.session_id)
                {
                    let approval_request_id = match &cp.yield_reason {
                        crate::runtime::checkpoint::YieldReason::ApprovalRequired {
                            approval_request_id,
                        } => Some(approval_request_id.clone()),
                        _ => None,
                    };
                    if let Some(ref rid) = approval_request_id {
                        let gateway_dir = config.agents_dir.join(".gateway");
                        if let Ok(mut report) =
                            crate::runtime::session_report::SessionReportWriter::open(
                                &gateway_dir,
                                &task.session_id,
                                &task.agent_id,
                            )
                        {
                            let _ = report.record_approval_resolved(
                                rid,
                                "stale",
                                &format!(
                                    "Approval timed out after {}s (checkpoint preserved)",
                                    timeout_secs
                                ),
                            );
                        }
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

            // Sessions whose LATEST checkpoint is WaitingForChild are mid-
            // transition into Paused — the parent is legitimately waiting for
            // async children, not stuck. Closes the narrow race between
            // save_yield_checkpoint and the Paused transition (#848, site 9).
            //
            // The exemption is BOUNDED: if the Running → Paused transition
            // died after the yield checkpoint was written, the task would sit
            // in Running, unqueued, skipped by this sweeper forever. When the
            // task is otherwise stale (past the stuck timeout AND its claim
            // heartbeat is stale), complete the interrupted pause here —
            // Paused + `paused_child_wait` checkpoint + dequeue — exactly what
            // the suspended_for_child_wait branch (spawn_task_execution) does.
            // The janitor (reconcile_paused_child_wait_tasks) then owns
            // re-evaluation. If the checkpoint cannot be read/verified,
            // conservatively skip this cycle rather than risk killing a
            // waiting parent.
            if !task.session_id.is_empty() {
                let waiting_for_child = match crate::runtime::checkpoint::load_latest_checkpoint(
                    &config,
                    &task.session_id,
                ) {
                    Ok(Some(cp)) => matches!(
                        cp.yield_reason,
                        crate::runtime::checkpoint::YieldReason::WaitingForChild { .. }
                    ),
                    Ok(None) => false,
                    Err(e) => {
                        tracing::warn!(
                            target: "workflow",
                            task_id = %task.task_id,
                            workflow_id = %wf_id,
                            session_id = %task.session_id,
                            error = %e,
                            "Stuck-task sweeper skipping task: latest checkpoint unreadable"
                        );
                        // Conservative: never touch a task whose checkpoint
                        // cannot be verified.
                        continue;
                    }
                };
                if waiting_for_child {
                    let updated_at = chrono::DateTime::parse_from_rfc3339(&task.updated_at)
                        .ok()
                        .map(|dt| dt.with_timezone(&chrono::Utc));
                    let elapsed_wait_secs = updated_at
                        .map(|ts| (now - ts).num_seconds() as u64)
                        .unwrap_or(0);
                    let claim_opt = workflow_store::load_task_claim(
                        &config,
                        store,
                        &wf_id,
                        &task.task_id,
                    )
                    .ok()
                    .flatten();
                    let claim_fresh = claim_opt.as_ref().map_or(false, |claim| {
                        !workflow_store::claim_is_stale(claim, stale_after_secs)
                    });
                    if elapsed_wait_secs < stale_after_secs || claim_fresh {
                        tracing::debug!(
                            target: "workflow",
                            task_id = %task.task_id,
                            workflow_id = %wf_id,
                            "Stuck-task sweeper skipping session with WaitingForChild checkpoint"
                        );
                        continue;
                    }

                    tracing::info!(
                        target: "workflow",
                        task_id = %task.task_id,
                        workflow_id = %wf_id,
                        session_id = %task.session_id,
                        elapsed_secs = elapsed_wait_secs,
                        "Completing interrupted child-wait pause for stale Running task (#848 race)"
                    );
                    if let Err(e) = workflow_store::update_task_run_status(
                        &config,
                        store,
                        &wf_id,
                        &task.task_id,
                        autonoetic_types::workflow::TaskRunStatus::Paused,
                        Some("paused: awaiting async child completion".to_string()),
                        None,
                        None,
                    ) {
                        tracing::warn!(
                            target: "workflow",
                            task_id = %task.task_id,
                            workflow_id = %wf_id,
                            error = %e,
                            "Failed to complete interrupted child-wait pause"
                        );
                        continue;
                    }
                    let _ = workflow_store::checkpoint_task(
                        &config,
                        store,
                        &wf_id,
                        &task.task_id,
                        "paused_child_wait".to_string(),
                        serde_json::json!({
                            "status": "paused",
                            "reason": "awaiting_async_child_completion",
                            "completed_by": "stuck_task_sweeper",
                        }),
                    );
                    let _ = workflow_store::dequeue_task(&config, store, &wf_id, &task.task_id);
                    continue;
                }
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

            // Respect heartbeat freshness: a task with a live claim is still
            // active, just slow. Only sweep once the claim heartbeat itself is
            // stale relative to the configured timeout.
            let claim_opt = workflow_store::load_task_claim(&config, store, &wf_id, &task.task_id)
                .ok()
                .flatten();
            let claim_fresh = claim_opt.as_ref().map_or(false, |claim| {
                !workflow_store::claim_is_stale(claim, stale_after_secs)
            });
            if claim_fresh {
                tracing::debug!(
                    target: "workflow",
                    task_id = %task.task_id,
                    workflow_id = %wf_id,
                    elapsed_secs = elapsed_secs,
                    "Stuck-task sweeper skipping task with fresh claim heartbeat"
                );
                continue;
            }

            let mut evidence = Vec::new();
            let mut diagnostics = Vec::new();
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
                                || digest.contains(
                                    autonoetic_types::session_outcome::SessionCloseOutcome::JsonRpcSpawnComplete.as_str(),
                                )
                            {
                                session_completed = true;
                                evidence
                                    .push("session digest contains completion markers".to_string());
                            }
                        }
                    }
                } else {
                    diagnostics.push("session directory does not exist".to_string());
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

            let no_evidence = !session_completed && evidence.is_empty();

            if no_evidence {
                // No completion evidence. Default behavior is to fail the task
                // so a genuinely hung/crashed execution is not silently reported
                // as a success. Operators can opt back into legacy succeed
                // behavior via `stuck_task_no_evidence_action: succeed`.
                let action = config.stuck_task_no_evidence_action;

                // Give a stale-claim task that might still be finalizing a
                // larger grace period (2x) before we declare it failed.
                if action == autonoetic_types::config::StuckTaskNoEvidenceAction::Fail
                    && elapsed_secs < stale_after_secs.saturating_mul(2)
                {
                    continue;
                }

                let heartbeat_age_secs = claim_opt.as_ref().and_then(|claim| {
                    chrono::DateTime::parse_from_rfc3339(&claim.heartbeat_at)
                        .ok()
                        .map(|dt| (now - dt.with_timezone(&chrono::Utc)).num_seconds())
                });

                let heartbeat_age_label = heartbeat_age_secs.map_or_else(
                    || "unknown".to_string(),
                    |secs| {
                        if secs >= 0 {
                            format!("{}s ago", secs)
                        } else {
                            format!("{}s in the future", -secs)
                        }
                    },
                );

                let (status, result_summary, checkpoint_status, event_type) = if action
                    == autonoetic_types::config::StuckTaskNoEvidenceAction::Fail
                {
                    let summary = format!(
                            "stuck_no_evidence: task running for {}s with no completion evidence; last heartbeat {}",
                            elapsed_secs,
                            heartbeat_age_label
                        );
                    (
                        autonoetic_types::workflow::TaskRunStatus::Failed,
                        summary,
                        "stuck_failed".to_string(),
                        "task.stuck".to_string(),
                    )
                } else {
                    let mut notes = evidence.clone();
                    notes.push("no evidence found — proceeding based on elapsed time".to_string());
                    notes.extend(diagnostics.clone());
                    let summary = format!(
                        "Auto-resolved stuck task: Succeeded (elapsed: {}s, evidence: {})",
                        elapsed_secs,
                        notes.join("; ")
                    );
                    (
                        autonoetic_types::workflow::TaskRunStatus::Succeeded,
                        summary,
                        "stuck_auto_resolved".to_string(),
                        "task.stuck_resolved".to_string(),
                    )
                };

                tracing::warn!(
                    target: "workflow",
                    task_id = %task.task_id,
                    workflow_id = %wf_id,
                    elapsed_secs = elapsed_secs,
                    evidence = ?evidence,
                    diagnostics = ?diagnostics,
                    "Stuck running task detected; resolving as {:?}",
                    status
                );

                let _ = workflow_store::update_task_run_status(
                    &config,
                    store,
                    &wf_id,
                    &task.task_id,
                    status,
                    Some(result_summary),
                    None,
                    None,
                );

                let _ = workflow_store::checkpoint_task(
                    &config,
                    store,
                    &wf_id,
                    &task.task_id,
                    checkpoint_status,
                    serde_json::json!({
                        "status": status.as_str(),
                        "evidence": evidence,
                        "diagnostics": diagnostics,
                        "session_completed": session_completed,
                        "elapsed_secs": elapsed_secs,
                        "heartbeat_age_secs": heartbeat_age_secs,
                    }),
                );

                let _ = workflow_store::dequeue_task(&config, store, &wf_id, &task.task_id);

                let event = autonoetic_types::workflow::WorkflowEventRecord {
                    event_id: format!("wevt-stuck-t-{}", &task.task_id),
                    workflow_id: wf_id.clone(),
                    event_type,
                    task_id: Some(task.task_id.clone()),
                    agent_id: Some(task.agent_id.clone()),
                    payload: serde_json::json!({
                        "evidence": evidence,
                        "diagnostics": diagnostics,
                        "session_completed": session_completed,
                        "elapsed_secs": elapsed_secs,
                        "heartbeat_age_secs": heartbeat_age_secs,
                        "resolved_status": status.as_str(),
                    }),
                    occurred_at: chrono::Utc::now().to_rfc3339(),
                };
                let _ = workflow_store::append_workflow_event(&config, store, &event);
                continue;
            }

            let mut all_notes = evidence.clone();
            all_notes.extend(diagnostics.clone());

            tracing::warn!(
                target: "workflow",
                task_id = %task.task_id,
                workflow_id = %wf_id,
                elapsed_secs = elapsed_secs,
                evidence = ?evidence,
                diagnostics = ?diagnostics,
                "Stuck running task detected; force-completing as Succeeded"
            );

            let result_summary = format!(
                "Auto-resolved stuck task: Succeeded (elapsed: {}s, evidence: {})",
                elapsed_secs,
                all_notes.join("; ")
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
                    "diagnostics": diagnostics,
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
                    "diagnostics": diagnostics,
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

    // #742: The orphan reaper no longer carries compensating exemptions for
    // children parked at approval gates or in-flight workflow tasks. The
    // lifecycle-state query (`find_orphaned_sessions`) only returns children
    // whose parent is `terminated:*`. Parents in `hibernated`, `awaiting_gate`,
    // or `active` protect their children by design — no transcript-status
    // heuristics or auxiliary lookups required. Stale children survive because
    // their parent is `hibernated`/`awaiting_gate`, not via a pending-approval
    // row coincidence (#722 Stage 2).

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
            // Task ids we mark Cancelled below; their live Tokio handles are
            // aborted after the loop (C1/#618 — close the zombie window where
            // the DB said Cancelled but the future kept running). Abort is
            // best-effort: a handle may not be registered (e.g. after a gateway
            // restart), in which case the DB cancel still stands.
            let mut cancelled_task_ids: Vec<String> = Vec::new();
            for mut task in tasks {
                if task.session_id != child_session_id {
                    continue;
                }
                if task.status.is_terminal() {
                    continue;
                }
                let was_awaiting_approval =
                    task.status == autonoetic_types::workflow::TaskRunStatus::AwaitingApproval;
                task.status = autonoetic_types::workflow::TaskRunStatus::Cancelled;
                task.updated_at = now_rfc.clone();
                task.result_summary =
                    Some("child_abandoned: parent session terminated (R+12)".to_string());
                let _ = crate::scheduler::workflow_store::save_task_run(
                    &config,
                    Some(store.as_ref()),
                    &task,
                );
                cancelled_task_ids.push(task.task_id.clone());
                if was_awaiting_approval {
                    let _ = crate::scheduler::approval::cancel_pending_approval_for_workflow_task(
                        &config,
                        Some(store.as_ref()),
                        &task.task_id,
                        "gateway",
                        "orphan_child_reaper",
                    );
                    let _ = crate::scheduler::workflow_store::sync_workflow_blocked_approval_status(
                        &config,
                        Some(store.as_ref()),
                        &task.workflow_id,
                    );
                }
                crate::scheduler::workflow_store::dequeue_task(
                    &config,
                    Some(store.as_ref()),
                    &task.workflow_id,
                    &task.task_id,
                )
                .ok();

                // Wake a parent blocked in `workflow.wait`. The reaper writes
                // the Cancelled status directly to the TaskRun, bypassing the
                // normal completion path that would notify the waiter — without
                // this signal the parent only discovers the cancellation via its
                // 5-second fallback poll or its own `timeout_secs` deadline.
                // `workflow.wait` registers its notifier under the waiting
                // session's id, which is the child task's `parent_session_id`.
                // (RFC: unit-test-runner-divergence-loop, Change 4)
                if !task.parent_session_id.is_empty() {
                    store.task_notify.notify_session(&task.parent_session_id);
                }
            }

            // Abort the live Tokio handles for the tasks we just cancelled in
            // the DB. Without this the future keeps running (the zombie window).
            // Scoped to this child's tasks in this workflow — never the whole
            // root, so live siblings under the same root are untouched.
            if !cancelled_task_ids.is_empty() {
                let aborted = execution
                    .active_executions()
                    .abort_workflow_tasks(&wf_id, &cancelled_task_ids);
                if aborted > 0 {
                    tracing::info!(
                        target: "orphan_reaper",
                        child_session_id = %child_session_id,
                        wf_id = %wf_id,
                        aborted,
                        "R+12: aborted in-flight task handles for abandoned child"
                    );
                }
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
        tracing::info!(
            target: "scheduler",
            interaction_id = %interaction.interaction_id,
            session_id = %interaction.session_id,
            agent_id = %interaction.agent_id,
            "Scheduling standalone session resume after answered user interaction"
        );

        let exec = execution.clone();
        let interaction_id = interaction.interaction_id.clone();
        tokio::spawn(async move {
            let result = exec
                .resume_session(
                    crate::execution::ResumeTrigger::InteractionAnswered {
                        interaction_id: interaction_id.clone(),
                    },
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
                        tracing::debug!(
                            target: "scheduler",
                            interaction_id = %interaction_id,
                            "Standalone interaction deferred: session is now waiting for approval"
                        );
                    } else if msg.contains("already claimed") {
                        tracing::debug!(
                            target: "scheduler",
                            interaction_id = %interaction_id,
                            "Standalone interaction resume already in progress"
                        );
                    } else {
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
                if let Some(retry_policy) =
                    workflow_store::retry_policy_from_metadata(queued_task.metadata.as_ref())
                {
                    existing.retry_policy = Some(retry_policy);
                }
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
                        event_id: autonoetic_types::id_format::short_random_id("wevt-"),
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
                retry_count: 0,
                last_failure_class: None,
                retry_policy: workflow_store::retry_policy_from_metadata(
                    queued_task.metadata.as_ref(),
                ),
                side_effect_state: None,
                dedupe_key: None,
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
                    event_id: autonoetic_types::id_format::short_random_id("wevt-"),
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

        // Mark the singleton slot as running so duplicate spawns see it as active.
        if let Some(gs) = store {
            if let Err(e) = gs.activate_singleton_task(
                &queued_task.workflow_id,
                &queued_task.agent_id,
                queued_task.metadata.as_ref().and_then(|m| {
                    m.get("_autonoetic_spawn_revision_id")
                        .and_then(|v| v.as_str())
                }),
                &queued_task.task_id,
            ) {
                tracing::warn!(
                    target: "singleton_dedup",
                    workflow_id = %queued_task.workflow_id,
                    task_id = %queued_task.task_id,
                    agent_id = %queued_task.agent_id,
                    error = %e,
                    "Failed to activate singleton slot"
                );
            }
        }

        // Spawn background execution
        let exec = execution.clone();
        let reg = execution.active_executions();
        let agent_id = queued_task.agent_id.clone();
        let message = queued_task.message.clone();
        let session_id = queued_task.child_session_id.clone();
        let source_id = queued_task.source_agent_id.clone();
        let metadata = queued_task.metadata.clone();
        let revision_id = metadata
            .as_ref()
            .and_then(|m| m.get("_autonoetic_spawn_revision_id"))
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned);
        let wf_id = queued_task.workflow_id.clone();
        let t_id = queued_task.task_id.clone();
        let cfg = config.clone();
        let cred_bindings = queued_task.credential_bindings.clone();

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
                    exec,
                    cfg,
                    wf_id,
                    t_id,
                    agent_id,
                    message,
                    session_id,
                    source_id,
                    metadata,
                    revision_id,
                    cred_bindings,
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

/// Truncate `s` to at most `max_bytes` UTF-8 bytes without splitting a
/// multi-byte character. Returns the prefix ending at the nearest char boundary
/// ≤ `max_bytes`.
fn truncate_to_byte_boundary(s: &str, max_bytes: usize) -> String {
    let end = s.len().min(max_bytes);
    let mut cut = end;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s[..cut].to_string()
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
    revision_id: Option<String>,
    credential_bindings: Vec<autonoetic_types::runtime_lock::LockedCredentialMount>,
) {
    let store = exec.gateway_store();
    let store = store.as_deref();
    // Load previous task checkpoint (if any) for resume context
    let prev_checkpoint = workflow_store::load_task_checkpoint(&cfg, store, &wf_id, &t_id)
        .ok()
        .flatten();
    let preview = truncate_to_byte_boundary(&message, 120);
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
            "message_preview": preview,
            "resuming_from": { "step": prev.step, "version": prev.version, "state": prev.state },
        })
    } else {
        serde_json::json!({
            "agent_id": agent_id,
            "session_id": session_id,
            "message_preview": preview,
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
        autonoetic_types::id_format::short_random_id("")
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

    // Cron rows injected from `auto_learning` config carry the synthetic owner
    // `gateway.auto-learning`, which is not an installed agent — resolving it
    // as a spawn source would fail every trigger. These jobs are
    // gateway-initiated, so they carry no source agent for the AgentSpawn
    // policy check. The `gateway.` id namespace is reserved (validate_agent_id),
    // so no installed agent can claim this owner id.
    let source_agent: Option<&str> =
        if source_id == crate::scheduler::auto_learning_jobs::AUTO_LEARNING_OWNER_ID {
            None
        } else {
            Some(&source_id)
        };

    let result = if let Some(ref rev_id) = revision_id {
        exec.spawn_agent_revision_once(
            &agent_id,
            Some(rev_id.as_str()),
            &message,
            &session_id,
            source_agent,
            false,
            None,
            metadata.as_ref(),
            Some(&wf_id),
            Some(&t_id),
            None,
            &credential_bindings,
        )
        .await
    } else {
        exec.spawn_agent_once(
            &agent_id,
            &message,
            &session_id,
            source_agent,
            false,
            None,
            metadata.as_ref(),
            Some(&wf_id),
            Some(&t_id),
            None,
            &credential_bindings,
        )
        .await
    };

    heartbeat.abort();
    let _ = heartbeat.await;

    match result {
        Ok(spawn_result) => {
            // Check if the turn was suspended at an approval gate (continuation already on disk).
            if let Some(ref request_id) = spawn_result.suspended_for_approval {
                let summary = format!("awaiting approval {}", request_id);

                // Load checkpoint to get approval details (tool name, etc.)
                let approval_metadata =
                    crate::runtime::checkpoint::load_latest_checkpoint(&cfg, &session_id)
                        .ok()
                        .flatten()
                        .and_then(|cp| {
                            let pending = cp.pending_tool_state?;
                            let tool_name = pending.pending_tool_call.tool_name.clone();
                            // Derive approval kind from tool name
                            let kind = if tool_name.contains("sandbox") {
                                "sandbox".to_string()
                            } else if tool_name.contains("install") {
                                "agent_install".to_string()
                            } else {
                                "tool_execution".to_string()
                            };
                            // Try to extract reason from approval_response
                            let reason = pending
                                .pending_tool_call
                                .approval_response
                                .as_ref()
                                .and_then(|resp| {
                                    serde_json::from_str::<serde_json::Value>(resp)
                                        .ok()
                                        .and_then(|v| {
                                            v.get("approval")
                                                .and_then(|a| a.get("reason"))
                                                .and_then(|r| r.as_str())
                                                .map(String::from)
                                        })
                                });

                            // Extract request_id from the yield reason
                            let request_id = match &cp.yield_reason {
                                crate::runtime::checkpoint::YieldReason::ApprovalRequired {
                                    approval_request_id,
                                } => Some(approval_request_id.clone()),
                                _ => None,
                            }?;

                            Some(workflow_store::ApprovalMetadata {
                                request_id,
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

            // Parent session suspended waiting for async child(ren) to complete
            // (the agent spawned async=true children then ended its turn —
            // Ri-0.14 / docs/AGENTS.md "Sequential / single child" pattern).
            // The session checkpoint is already labelled `WaitingForChild` and
            // the auto-resume machinery (signal-triggered) will re-wake this
            // task when a child transitions. Until then, the task MUST stay
            // non-terminal: marking it `Succeeded` here would (a) fire the
            // workflow child-resolved notification prematurely, (b) cause the
            // root planner to conclude the install pipeline is done when its
            // own follow-up steps (smoke-test, promote, …) never ran, and
            // (c) leak the candidate-revision-only state to operators as
            // "completed" (#845).
            if spawn_result.suspended_for_child_wait {
                let summary = Some("paused: awaiting async child completion".to_string());
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
                        "Failed to persist paused (child wait) workflow task status"
                    );
                }
                let _ = workflow_store::checkpoint_task(
                    &cfg,
                    store,
                    &wf_id,
                    &t_id,
                    "paused_child_wait".to_string(),
                    serde_json::json!({
                        "status": "paused",
                        "reason": "awaiting_async_child_completion",
                    }),
                );
                let _ = workflow_store::dequeue_task(&cfg, store, &wf_id, &t_id);
                tracing::info!(
                    target: "workflow",
                    task_id = %t_id,
                    "Task paused: parent ended turn with pending async children (WaitingForChild / Ri-0.14)"
                );
                finish_active_row("stopped");
                return;
            }

            let summary = spawn_result.assistant_reply.as_ref().map(|s| {
                const MAX: usize = 512;
                if s.chars().count() <= MAX {
                    s.clone()
                } else {
                    let safe: String = s.chars().take(MAX).collect();
                    format!("{}…", safe)
                }
            });

            // RFC #776 Part B.1: existence check — verify declared
            // expected_outputs resolve to produced content/artifact handles.
            // Existence only, never quality (invariant 5). The check result
            // is recorded on the task metadata so build_child_state_notification
            // can stamp OutputContractUnmet.
            {
                let task_result = workflow_store::load_task_run(
                    &cfg, store, &wf_id, &t_id,
                );
                let task = match task_result {
                    Ok(Some(t)) => Some(t),
                    Ok(None) => None,
                    Err(e) => {
                        tracing::warn!(
                            target: "workflow",
                            task_id = %t_id,
                            error = %e,
                            "Failed to load task for output contract check — enforcement skipped"
                        );
                        None
                    }
                };
                if let Some(mut task) = task {
                    let expected: Vec<String> = task.metadata
                        .as_ref()
                        .and_then(|m| m.get("expected_outputs"))
                        .and_then(|v| v.as_array())
                        .map(|arr| arr.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect())
                        .unwrap_or_default();

                    if !expected.is_empty() {
                        let content_names: Vec<String> = spawn_result.files
                            .iter()
                            .map(|f| f.name.clone())
                            .collect();
                        let artifact_files: Vec<String> = spawn_result.artifacts
                            .iter()
                            .flat_map(|a| a.files.iter().cloned())
                            .collect();
                        let unmet = workflow_store::check_output_contract(
                            &expected, &content_names, &artifact_files,
                        );
                        if !unmet.is_empty() {
                            tracing::info!(
                                target: "workflow",
                                task_id = %t_id,
                                unmet = ?unmet,
                                expected = ?expected,
                                "Output contract check: some expected outputs missing"
                            );
                        }
                        workflow_store::record_output_contract_check(&mut task, unmet);
                        if let Err(e) = workflow_store::save_task_run(&cfg, store, &task) {
                            tracing::warn!(
                                target: "workflow",
                                task_id = %t_id,
                                error = %e,
                                "Failed to persist output contract check — stamping may be skipped"
                            );
                        }
                    }
                }
            }

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
                    "result_summary": spawn_result.assistant_reply.as_ref().map(|s| {
                        let max = 200;
                        if s.chars().count() <= max {
                            s.clone()
                        } else {
                            s.chars().take(max).collect::<String>()
                        }
                    }),
                }),
            );
            let _ = workflow_store::dequeue_task(&cfg, store, &wf_id, &t_id);
            finish_active_row("stopped");
        }
        Err(e) => {
            let error_str = e.to_string();

            // ── Overflow-aware retry ──────────────────────────────────
            // Detect context overflow errors and retry exactly once with
            // an aggressive governor pipeline. A second overflow is terminal.
            if crate::scheduler::overflow_classifier::is_context_overflow(&e) {
                let already_retried = prev_checkpoint
                    .as_ref()
                    .and_then(|cp| cp.state.get("overflow_recovery_attempted"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                if !already_retried {
                    tracing::warn!(
                        target: "workflow",
                        task_id = %t_id,
                        error = %error_str,
                        "Context overflow detected — retrying once with aggressive governor"
                    );
                    if let Some(ref gs) = store {
                        let _ = gs.create_causal_event(&CausalEventRecord {
                            event_id: uuid::Uuid::new_v4().to_string(),
                            agent_id: agent_id.clone(),
                            session_id: session_id.clone(),
                            turn_id: None,
                            event_seq: 0,
                            timestamp: Utc::now().to_rfc3339(),
                            category: "agent.process".to_string(),
                            action: "overflow_retry_started".to_string(),
                            status: "SUCCESS".to_string(),
                            enforced_rules: vec![],
                            target: None,
                            payload: Some(
                                serde_json::json!({
                                    "task_id": t_id,
                                    "workflow_id": wf_id,
                                    "error": error_str,
                                })
                                .to_string(),
                            ),
                            payload_ref: None,
                            evidence_ref: None,
                            reason: Some(
                                "Context overflow detected, retrying once with aggressive governor"
                                    .to_string(),
                            ),
                        });
                    }
                    let _ = workflow_store::checkpoint_task(
                        &cfg,
                        store,
                        &wf_id,
                        &t_id,
                        "failed".to_string(),
                        serde_json::json!({
                            "status": "failed",
                            "error": error_str,
                            "overflow_recovery_attempted": true,
                        }),
                    );
                    // Tag the task metadata so the governor knows to use the
                    // aggressive reduction pipeline on retry. Merge into any
                    // existing metadata rather than replacing it outright.
                    let existing_meta = workflow_store::load_task_run(&cfg, store, &wf_id, &t_id)
                        .ok()
                        .flatten()
                        .and_then(|t| t.metadata)
                        .unwrap_or(serde_json::json!({}));
                    let merged = if let serde_json::Value::Object(mut m) = existing_meta {
                        m.insert(
                            "overflow_recovery".to_string(),
                            serde_json::Value::Bool(true),
                        );
                        serde_json::Value::Object(m)
                    } else {
                        serde_json::json!({ "overflow_recovery": true })
                    };
                    let _ = workflow_store::update_task_run_metadata(
                        &cfg, store, &wf_id, &t_id, merged,
                    );
                    // Set to Runnable so the scheduler re-queues this task
                    let _ = workflow_store::update_task_run_status(
                        &cfg,
                        store,
                        &wf_id,
                        &t_id,
                        autonoetic_types::workflow::TaskRunStatus::Runnable,
                        Some("overflow_recovery_retry".to_string()),
                        None,
                        None,
                    );
                    let _ = workflow_store::dequeue_task(&cfg, store, &wf_id, &t_id);
                    finish_active_row("stopped");
                    return;
                }

                tracing::warn!(
                    target: "workflow",
                    task_id = %t_id,
                    error = %error_str,
                    "Context overflow retry exhausted — marking terminal"
                );
                if let Some(ref gs) = store {
                    let _ = gs.create_causal_event(&CausalEventRecord {
                        event_id: uuid::Uuid::new_v4().to_string(),
                        agent_id: agent_id.clone(),
                        session_id: session_id.clone(),
                        turn_id: None,
                        event_seq: 0,
                        timestamp: Utc::now().to_rfc3339(),
                        category: "agent.process".to_string(),
                        action: "overflow_retry_exhausted".to_string(),
                        status: "ERROR".to_string(),
                        enforced_rules: vec![],
                        target: None,
                        payload: Some(
                            serde_json::json!({
                                "task_id": t_id,
                                "workflow_id": wf_id,
                                "error": error_str,
                            })
                            .to_string(),
                        ),
                        payload_ref: None,
                        evidence_ref: None,
                        reason: Some(
                            "Context overflow retry exhausted — marking terminal".to_string(),
                        ),
                    });
                }
                // Fall through to normal failure path with terminal classification
                let terminal_error =
                    format!("context_overflow_terminal: task={} {}", t_id, error_str);
                let _ = workflow_store::update_task_run_status(
                    &cfg,
                    store,
                    &wf_id,
                    &t_id,
                    autonoetic_types::workflow::TaskRunStatus::Failed,
                    Some(terminal_error.clone()),
                    None,
                    None,
                );
                let _ = workflow_store::checkpoint_task(
                    &cfg,
                    store,
                    &wf_id,
                    &t_id,
                    "failed".to_string(),
                    serde_json::json!({
                        "status": "failed",
                        "error": terminal_error,
                        "overflow_recovery_exhausted": true,
                    }),
                );
                let _ = workflow_store::dequeue_task(&cfg, store, &wf_id, &t_id);
                finish_active_row("stopped");
                return;
            }

            let retry_decision = workflow_store::load_task_run(&cfg, store, &wf_id, &t_id)
                .ok()
                .flatten()
                .map(|task| {
                    workflow_store::evaluate_stage_retry(
                        &task,
                        autonoetic_types::workflow::TaskRunStatus::Failed,
                        Some(error_str.as_str()),
                    )
                });
            if let Some(ref decision) = retry_decision {
                if decision.retry_scheduled {
                    if let Err(inner) = workflow_store::schedule_task_stage_retry(
                        &cfg,
                        store,
                        &wf_id,
                        &t_id,
                        Some(error_str.clone()),
                        decision,
                    ) {
                        tracing::warn!(
                            target: "workflow",
                            task_id = %t_id,
                            error = %inner,
                            "Failed to persist stage-local retry scheduling"
                        );
                    } else {
                        tracing::info!(
                            target: "workflow",
                            task_id = %t_id,
                            retry_count = decision.next_retry_count,
                            "Stage-local retry scheduled"
                        );
                        let _ = workflow_store::checkpoint_task(
                            &cfg,
                            store,
                            &wf_id,
                            &t_id,
                            "retry_scheduled".to_string(),
                            serde_json::json!({
                                "status": "runnable",
                                "retry_count": decision.next_retry_count,
                                "failure_class": decision.failure.as_ref().and_then(|failure| failure.failure_class).and_then(|value| serde_json::to_value(value).ok()),
                                "retry_advice": decision.failure.as_ref().and_then(|failure| failure.retry_advice).and_then(|value| serde_json::to_value(value).ok()),
                                "error": error_str,
                            }),
                        );
                        let _ = workflow_store::dequeue_task(&cfg, store, &wf_id, &t_id);
                        finish_active_row("stopped");
                        return;
                    }
                }
            }

            if let Err(inner) = workflow_store::update_task_run_status(
                &cfg,
                store,
                &wf_id,
                &t_id,
                autonoetic_types::workflow::TaskRunStatus::Failed,
                Some(error_str.clone()),
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
                serde_json::json!({ "status": "failed", "error": error_str }),
            );
            // Append validation errors to digest for better visibility
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

/// Safety-net janitor for the workflow suspend/resume machinery.
///
/// Thin scheduler wrapper around
/// `workflow_store::reconcile_paused_child_wait_tasks` (which holds the logic
/// and the unit tests). Runs every tick — see the store-level function for
/// the full rationale.
pub async fn reconcile_paused_child_wait_tasks(
    execution: Arc<crate::execution::GatewayExecutionService>,
) -> anyhow::Result<()> {
    let config = execution.config();
    let store = execution.gateway_store();
    workflow_store::reconcile_paused_child_wait_tasks(&config, store.as_deref())
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

            if workflow_store::queued_task_exists(&config, store, &wf_id, &task.task_id) {
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

            // Child-wait wakes resume the session from its WaitingForChild
            // checkpoint, which already carries the full conversation history.
            // Re-injecting the original kickoff message would duplicate the
            // instruction in that history and risks re-executing side effects
            // — send a wake notice instead.
            let is_child_wait_wake = workflow_store::load_task_checkpoint(
                &config,
                store,
                &wf_id,
                &task.task_id,
            )
            .ok()
            .flatten()
            .map(|cp| cp.step == "paused_child_wait")
            .unwrap_or(false);
            let message = if is_child_wait_wake {
                "[gateway child state notification] All spawned child tasks have resolved. \
                 Continue and produce your final result."
                    .to_string()
            } else {
                task.message
                    .clone()
                    .unwrap_or_else(|| format!("Resume after approval: {}", task.session_id))
            };

            let queued = autonoetic_types::workflow::QueuedTaskRun {
                task_id: task.task_id.clone(),
                workflow_id: wf_id.clone(),
                agent_id: task.agent_id.clone(),
                message,
                child_session_id: task.session_id.clone(),
                parent_session_id: task.parent_session_id.clone(),
                source_agent_id: task.source_agent_id.clone().unwrap_or_default(),
                metadata: task.metadata.clone(),
                join_group: task.join_group.clone(),
                blocks_planner,
                enqueued_at: chrono::Utc::now().to_rfc3339(),
                credential_bindings: vec![],
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
    router: Option<Arc<crate::router::JsonRpcRouter>>,
) -> anyhow::Result<()> {
    let pending = store.list_pending_notifications()?;
    if pending.is_empty() {
        return Ok(());
    }

    let port = execution.config().port;
    let timeout_secs = execution.config().signal_delivery_timeout_secs;

    // Debounce: track sessions that already received a child-state
    // notification in this pump cycle. When multiple children complete
    // near-simultaneously (e.g. parallel fan-out without workflow_wait),
    // each would otherwise trigger a separate planner wake. The first
    // wake carries the full workflow status (injected at resume time by
    // gateway_signal_turn_start_context), so subsequent child-state
    // notifications for the same session are redundant. Coalesce them:
    // mark as delivered without sending.
    let mut child_notified_sessions: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    // Cache terminal-workflow checks for this pump cycle to avoid re-reading
    // workflow.json from disk for every notification sharing the same workflow_id.
    let mut terminal_workflows: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for n in pending {
        // Suppress notifications whose workflow has already reached a terminal
        // state. Stale child-state / join-satisfied signals must not wake a
        // root session that has already emitted its final response.
        if let Some(ref workflow_id) = n.workflow_id {
            let is_terminal = if terminal_workflows.contains(workflow_id) {
                true
            } else {
                match crate::scheduler::workflow_store::is_workflow_terminal(
                    execution.config().as_ref(),
                    Some(store),
                    workflow_id,
                ) {
                    Ok(true) => {
                        terminal_workflows.insert(workflow_id.clone());
                        true
                    }
                    Ok(false) => false,
                    Err(e) => {
                        tracing::warn!(
                            notification_id = %n.notification_id,
                            workflow_id = %workflow_id,
                            error = %e,
                            "Failed to check workflow terminal status; delivering notification anyway"
                        );
                        false
                    }
                }
            };
            if is_terminal {
                tracing::info!(
                    notification_id = %n.notification_id,
                    workflow_id = %workflow_id,
                    notification_type = ?n.notification_type,
                    "Suppressing notification for terminal workflow"
                );
                store.update_notification_status(
                    &n.notification_id,
                    autonoetic_types::notification::NotificationStatus::Suppressed,
                )?;
                continue;
            }
        }

        // Coalesce redundant child-state notifications.
        if n.notification_type
            == autonoetic_types::notification::NotificationType::ChildStateNotification
            && child_notified_sessions.contains(&n.target_session_id)
        {
            tracing::info!(
                notification_id = %n.notification_id,
                target_session_id = %n.target_session_id,
                "Coalescing redundant child-state notification (another child-state signal for this session was already delivered in this cycle)"
            );
            store.update_notification_status(
                &n.notification_id,
                autonoetic_types::notification::NotificationStatus::Delivered,
            )?;
            continue;
        }

        // Map NotificationRecord payload to Signal.
        // Only current Signal payloads are accepted for deterministic delivery.
        let signal = match n.notification_type {
            autonoetic_types::notification::NotificationType::ApprovalResolved => {
                serde_json::from_value::<crate::scheduler::signal::Signal>(n.payload.clone()).ok()
            }
            autonoetic_types::notification::NotificationType::WorkflowJoinSatisfied => {
                serde_json::from_value::<crate::scheduler::signal::Signal>(n.payload.clone()).ok()
            }
            autonoetic_types::notification::NotificationType::ChildStateNotification => {
                serde_json::from_value::<crate::scheduler::signal::Signal>(n.payload.clone()).ok()
            }
            autonoetic_types::notification::NotificationType::AgentMessage => {
                serde_json::from_value::<crate::scheduler::signal::Signal>(n.payload.clone()).ok()
            }
            autonoetic_types::notification::NotificationType::AdminProposal => None,
            autonoetic_types::notification::NotificationType::ConstitutionalProposal => None,
            autonoetic_types::notification::NotificationType::AnomalyFlag => None,
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
            if let Some(router) = router.as_ref() {
                let request = crate::scheduler::signal::build_delivery_request(
                    &pending_signal,
                    &n.target_session_id,
                );
                let response = router.dispatch(request).await;
                if let Some(error) = response.error {
                    let e = anyhow::anyhow!("Signal delivery failed: {}", error.message);
                    tracing::warn!(notification_id = %n.notification_id, error = %e, "Failed to deliver signal in-process");
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
                    if n.notification_type
                        == autonoetic_types::notification::NotificationType::ChildStateNotification
                    {
                        child_notified_sessions.insert(n.target_session_id.clone());
                    }
                }
            } else if let Err(e) = crate::scheduler::signal::deliver_signal(
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

                // Track for coalescing: if this was a child-state
                // notification, subsequent ones for the same session
                // will be coalesced.
                if n.notification_type
                    == autonoetic_types::notification::NotificationType::ChildStateNotification
                {
                    child_notified_sessions.insert(n.target_session_id.clone());
                }
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

        // The cron path claims the job (advancing next_run_at to the next
        // occurrence) so a later tick can't re-fire it. The claim is the
        // cron-vs-cron double-fire guard; the manual trigger path relies on
        // the in-flight task guard instead.
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

        // The claim already advanced next_run_at, so pass the computed
        // occurrence through to avoid a redundant store write.
        if let Err(e) = enqueue_scheduled_job_fire(
            &config,
            store.as_ref(),
            &claimed,
            now,
            /* manual */ false,
            /* next_run_at_override */ Some(&next_run_at),
        ) {
            tracing::warn!(
                target: "scheduler",
                job_id = %claimed.job_id,
                error = %e,
                "Failed to fire scheduled job; recording error for backoff"
            );
            let backoff_secs = 60;
            let retry_at =
                (chrono::Utc::now() + chrono::Duration::seconds(backoff_secs)).to_rfc3339();
            let _ = store.advance_next_run(
                &claimed.job_id,
                &retry_at,
                None,
                Some(&format!("Fire failed: {}", e)),
            );
        }
    }

    Ok(())
}

/// Materialize a fire of a scheduled job: advance the schedule, create-or-reuse
/// the `sched-{job_id}` WorkflowRun, enqueue a fresh task, and append a
/// `scheduled_job.triggered` workflow event.
///
/// This is the single fire code path, used by both the cron tick
/// (`process_due_scheduled_jobs`) and the operator `scheduled_jobs.trigger`
/// JSON-RPC method. The agent session itself is spawned later by
/// `process_queued_workflow_tasks` draining the queue.
///
/// `next_run_at_override` lets the cron caller pass the next occurrence it
/// already computed during claim, avoiding a redundant advance. When `None`
/// (the manual trigger path), the schedule is advanced to the next cron
/// occurrence from `now` so a later cron tick won't double-fire.
pub fn enqueue_scheduled_job_fire(
    config: &autonoetic_types::config::GatewayConfig,
    store: &crate::scheduler::gateway_store::GatewayStore,
    job: &autonoetic_types::scheduled_job::ScheduledJob,
    now: chrono::DateTime<chrono::Utc>,
    manual: bool,
    next_run_at_override: Option<&str>,
) -> anyhow::Result<autonoetic_types::scheduled_job::ScheduledJobTriggerEvent> {
    let now_rfc = now.to_rfc3339();

    // Compute (or accept) the next cron occurrence and advance the schedule so
    // the regular tick does not re-fire the same job.
    let next_run_at: String = match next_run_at_override {
        Some(s) => s.to_string(),
        None => {
            let cron = crate::scheduler::cron_parser::parse_schedule(&job.cron_expr)?;
            let next = crate::scheduler::cron_parser::next_occurrence(&cron, now)
                .ok_or_else(|| anyhow::anyhow!("No future occurrence for cron '{}'", job.cron_expr))?;
            let s = next.to_rfc3339();
            store.advance_next_run(&job.job_id, &s, Some(&now_rfc), None)?;
            s
        }
    };

    let workflow_id = format!("sched-{}", &job.job_id);
    let task_id = format!(
        "task-{}-{}",
        &job.job_id,
        autonoetic_types::id_format::short_random_id("")
    );

    // Ensure the WorkflowRun exists — `enqueue_task` errors if it does not, and
    // it performs all run-state mutation (loading, appending to
    // queued_task_ids, and saving) itself. We therefore do NOT save `run` back
    // after enqueue: that would overwrite the run that `enqueue_task` just
    // persisted with this stale pre-enqueue snapshot, dropping the newly queued
    // task from `queued_task_ids`.
    match workflow_store::load_workflow_run(config, Some(store), &workflow_id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            let new_run = autonoetic_types::workflow::WorkflowRun {
                workflow_id: workflow_id.clone(),
                root_session_id: job.root_session_id.clone(),
                lead_agent_id: job.owner_agent_id.clone(),
                status: autonoetic_types::workflow::WorkflowRunStatus::Active,
                created_at: now_rfc.clone(),
                updated_at: now_rfc.clone(),
                active_task_ids: Vec::new(),
                queued_task_ids: Vec::new(),
                join_policy: autonoetic_types::workflow::JoinPolicy::AllOf,
                join_task_ids: Vec::new(),
                active_plan_ref: None,
                reactivated_for_root_spawn: false,
            };
            workflow_store::save_workflow_run(config, Some(store), &new_run)?;
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "Failed to load workflow run for {}: {}",
                workflow_id,
                e
            ));
        }
    }

    let mut metadata = serde_json::json!({
        "scheduled_job_id": job.job_id,
        "scheduled_next_run_at": next_run_at.clone(),
    });
    if manual {
        metadata["manual"] = serde_json::json!(true);
    }

    let queued = autonoetic_types::workflow::QueuedTaskRun {
        task_id: task_id.clone(),
        workflow_id: workflow_id.clone(),
        agent_id: format!("{}@{}", job.target_agent_id, job.target_revision_id),
        message: job.message.clone(),
        child_session_id: format!("sched-child-{}", &job.job_id),
        parent_session_id: job.root_session_id.clone(),
        source_agent_id: job.owner_agent_id.clone(),
        metadata: Some(metadata),
        join_group: None,
        blocks_planner: false,
        enqueued_at: now_rfc.clone(),
        credential_bindings: vec![],
    };

    if let Err(e) = workflow_store::enqueue_task(config, Some(store), &queued) {
        return Err(anyhow::anyhow!("Failed to enqueue task: {}", e));
    }

    let trigger_event = autonoetic_types::workflow::WorkflowEventRecord {
        event_id: format!("wevt-sched-{}", &task_id),
        workflow_id: workflow_id.clone(),
        event_type: "scheduled_job.triggered".to_string(),
        task_id: Some(task_id.clone()),
        agent_id: Some(job.target_agent_id.clone()),
        payload: serde_json::json!({
            "job_id": job.job_id,
            "owner_agent_id": job.owner_agent_id,
            "scheduled_for": next_run_at,
            "manual": manual,
        }),
        occurred_at: now_rfc.clone(),
    };
    let _ = workflow_store::append_workflow_event(config, Some(store), &trigger_event);

    tracing::info!(
        target: "scheduler",
        job_id = %job.job_id,
        workflow_id = %workflow_id,
        task_id = %task_id,
        manual,
        "Scheduled job fired"
    );

    Ok(autonoetic_types::scheduled_job::ScheduledJobTriggerEvent {
        event_id: format!("wevt-sched-{}", &task_id),
        job_id: job.job_id.clone(),
        workflow_id,
        task_id,
        root_session_id: job.root_session_id.clone(),
        triggered_at: now_rfc,
        scheduled_for: next_run_at,
    })
}

#[cfg(test)]
mod stuck_task_tests {
    use super::*;
    use autonoetic_types::config::{GatewayConfig, StuckTaskNoEvidenceAction};
    use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowEventRecord};
    use std::path::Path;
    use tempfile::tempdir;

    async fn run_approval_timeout_sweeper(config: &GatewayConfig) {
        let gateway_dir = config.agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store =
            Arc::new(crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap());
        let exec = Arc::new(crate::execution::GatewayExecutionService::new(
            config.clone(),
            Some(store),
        ));
        check_approval_timeouts(exec).await.unwrap();
    }

    fn test_config(agents_dir: &Path) -> GatewayConfig {
        GatewayConfig {
            agents_dir: agents_dir.to_path_buf(),
            ..GatewayConfig::default()
        }
    }

    fn old_rfc3339(secs_ago: u64) -> String {
        (chrono::Utc::now() - chrono::Duration::seconds(secs_ago as i64)).to_rfc3339()
    }

    fn stale_claim(
        config: &GatewayConfig,
        workflow_id: &str,
        task_id: &str,
        heartbeat_secs_ago: u64,
    ) {
        let claim = crate::scheduler::gateway_store::TaskExecutionClaim {
            workflow_id: workflow_id.to_string(),
            task_id: task_id.to_string(),
            scheduler_instance_id: "test-instance".to_string(),
            claimed_at: old_rfc3339(heartbeat_secs_ago + 1),
            heartbeat_at: old_rfc3339(heartbeat_secs_ago),
        };
        crate::scheduler::store::write_json_file(
            &workflow_store::task_claim_path(config, workflow_id, task_id),
            &claim,
        )
        .unwrap();
    }

    fn fresh_claim(config: &GatewayConfig, workflow_id: &str, task_id: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        let claim = crate::scheduler::gateway_store::TaskExecutionClaim {
            workflow_id: workflow_id.to_string(),
            task_id: task_id.to_string(),
            scheduler_instance_id: "test-instance".to_string(),
            claimed_at: now.clone(),
            heartbeat_at: now,
        };
        crate::scheduler::store::write_json_file(
            &workflow_store::task_claim_path(config, workflow_id, task_id),
            &claim,
        )
        .unwrap();
    }

    fn make_running_task(
        wf_id: &str,
        task_id: &str,
        session_id: &str,
        updated_secs_ago: u64,
    ) -> TaskRun {
        TaskRun {
            task_id: task_id.to_string(),
            workflow_id: wf_id.to_string(),
            agent_id: "agent".to_string(),
            session_id: session_id.to_string(),
            parent_session_id: "root".to_string(),
            status: TaskRunStatus::Running,
            created_at: old_rfc3339(updated_secs_ago + 1),
            updated_at: old_rfc3339(updated_secs_ago),
            source_agent_id: None,
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        }
    }

    async fn run_sweeper(config: &GatewayConfig) {
        let gateway_dir = config.agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store =
            Arc::new(crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap());
        let exec = Arc::new(crate::execution::GatewayExecutionService::new(
            config.clone(),
            Some(store),
        ));
        check_stuck_running_tasks(exec).await.unwrap();
    }

    #[tokio::test]
    async fn no_evidence_and_stale_heartbeat_fails() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let mut cfg = test_config(&agents);
        cfg.stuck_task_timeout_secs = Some(10);

        let wf =
            workflow_store::ensure_workflow_for_root_session(&cfg, None, "stuck-fail-root", None)
                .unwrap();
        let task = make_running_task(&wf.workflow_id, "task-stuck", "stuck-fail-root/child", 30);
        workflow_store::save_task_run(&cfg, None, &task).unwrap();
        stale_claim(&cfg, &wf.workflow_id, "task-stuck", 30);

        run_sweeper(&cfg).await;

        let updated = workflow_store::load_task_run(&cfg, None, &wf.workflow_id, "task-stuck")
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, TaskRunStatus::Failed);
        let summary = updated.result_summary.as_deref().unwrap();
        assert!(summary.starts_with("stuck_no_evidence"));

        let events = workflow_store::load_workflow_events(&cfg, None, &wf.workflow_id).unwrap();
        let stuck_events: Vec<&WorkflowEventRecord> = events
            .iter()
            .filter(|e| e.event_type == "task.stuck")
            .collect();
        assert_eq!(stuck_events.len(), 1);
        assert_eq!(
            stuck_events[0].payload["resolved_status"].as_str(),
            Some("failed")
        );
    }

    #[tokio::test]
    async fn manifest_evidence_still_succeeds() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let mut cfg = test_config(&agents);
        cfg.stuck_task_timeout_secs = Some(10);

        let wf =
            workflow_store::ensure_workflow_for_root_session(&cfg, None, "stuck-ok-root", None)
                .unwrap();
        let task = make_running_task(&wf.workflow_id, "task-ok", "stuck-ok-root/child", 15);
        workflow_store::save_task_run(&cfg, None, &task).unwrap();
        stale_claim(&cfg, &wf.workflow_id, "task-ok", 15);

        // Create session manifest showing completed status as evidence.
        let gateway_dir = crate::execution::gateway_root_dir(&cfg);
        let session_dir = gateway_dir.join("sessions").join(&task.session_id);
        std::fs::create_dir_all(&session_dir).unwrap();
        crate::scheduler::store::write_json_file(
            &session_dir.join("manifest.json"),
            &serde_json::json!({"visibility": {"status": "completed"}}),
        )
        .unwrap();

        run_sweeper(&cfg).await;

        let updated = workflow_store::load_task_run(&cfg, None, &wf.workflow_id, "task-ok")
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, TaskRunStatus::Succeeded);
        let events = workflow_store::load_workflow_events(&cfg, None, &wf.workflow_id).unwrap();
        assert!(events.iter().any(|e| e.event_type == "task.stuck_resolved"));
    }

    #[tokio::test]
    async fn fresh_heartbeat_is_not_swept() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let mut cfg = test_config(&agents);
        cfg.stuck_task_timeout_secs = Some(10);

        let wf =
            workflow_store::ensure_workflow_for_root_session(&cfg, None, "stuck-fresh-root", None)
                .unwrap();
        let task = make_running_task(&wf.workflow_id, "task-fresh", "stuck-fresh-root/child", 15);
        workflow_store::save_task_run(&cfg, None, &task).unwrap();
        fresh_claim(&cfg, &wf.workflow_id, "task-fresh");

        run_sweeper(&cfg).await;

        let updated = workflow_store::load_task_run(&cfg, None, &wf.workflow_id, "task-fresh")
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, TaskRunStatus::Running);
    }

    #[tokio::test]
    async fn legacy_succeed_config_force_completes() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let mut cfg = test_config(&agents);
        cfg.stuck_task_timeout_secs = Some(10);
        cfg.stuck_task_no_evidence_action = StuckTaskNoEvidenceAction::Succeed;

        let wf =
            workflow_store::ensure_workflow_for_root_session(&cfg, None, "stuck-legacy-root", None)
                .unwrap();
        let task = make_running_task(
            &wf.workflow_id,
            "task-legacy",
            "stuck-legacy-root/child",
            15,
        );
        workflow_store::save_task_run(&cfg, None, &task).unwrap();
        stale_claim(&cfg, &wf.workflow_id, "task-legacy", 15);

        run_sweeper(&cfg).await;

        let updated = workflow_store::load_task_run(&cfg, None, &wf.workflow_id, "task-legacy")
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, TaskRunStatus::Succeeded);
        let events = workflow_store::load_workflow_events(&cfg, None, &wf.workflow_id).unwrap();
        assert!(events.iter().any(|e| e.event_type == "task.stuck_resolved"));
    }

    #[tokio::test]
    async fn failed_stuck_task_still_satisfies_join() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let mut cfg = test_config(&agents);
        cfg.stuck_task_timeout_secs = Some(10);

        let wf =
            workflow_store::ensure_workflow_for_root_session(&cfg, None, "stuck-join-root", None)
                .unwrap();
        let task = make_running_task(
            &wf.workflow_id,
            "task-join-fail",
            "stuck-join-root/child",
            30,
        );
        workflow_store::save_task_run(&cfg, None, &task).unwrap();
        stale_claim(&cfg, &wf.workflow_id, "task-join-fail", 30);

        let mut run = workflow_store::load_workflow_run(&cfg, None, &wf.workflow_id)
            .unwrap()
            .unwrap();
        run.join_task_ids = vec!["task-join-fail".to_string()];
        workflow_store::save_workflow_run(&cfg, None, &run).unwrap();

        assert!(!workflow_store::check_join_condition(&cfg, None, &wf.workflow_id).unwrap());

        run_sweeper(&cfg).await;

        assert!(workflow_store::check_join_condition(&cfg, None, &wf.workflow_id).unwrap());
    }

    #[tokio::test]
    async fn approval_timeout_marks_task_stale_and_preserves_checkpoint() {
        use crate::runtime::checkpoint::{save_checkpoint, SessionCheckpoint, YieldReason};
        use autonoetic_types::background::{ApprovalLevel, ApprovalRequest, ScheduledAction};

        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let mut cfg = test_config(&agents);
        cfg.approval_timeout_secs = 60;

        let wf = workflow_store::ensure_workflow_for_root_session(
            &cfg,
            None,
            "approval-timeout-root",
            None,
        )
        .unwrap();
        let task_id = "task-awaiting";
        let session_id = format!("approval-timeout-root/child-{task_id}");
        let task = TaskRun {
            task_id: task_id.to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: session_id.clone(),
            parent_session_id: "approval-timeout-root".to_string(),
            status: TaskRunStatus::AwaitingApproval,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            source_agent_id: None,
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        workflow_store::save_task_run(&cfg, None, &task).unwrap();

        let mut wf_run = workflow_store::load_workflow_run(&cfg, None, &wf.workflow_id)
            .unwrap()
            .unwrap();
        wf_run.status = autonoetic_types::workflow::WorkflowRunStatus::BlockedApproval;
        wf_run.active_task_ids = vec![task_id.to_string()];
        workflow_store::save_workflow_run(&cfg, None, &wf_run).unwrap();

        let approval_request_id = "apr-timeout-01";
        let mut approval = ApprovalRequest {
            request_id: approval_request_id.to_string(),
            agent_id: "coder.default".to_string(),
            session_id: session_id.clone(),
            action: ScheduledAction::SandboxExec {
                command: "echo hi".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
                detected_hosts: None,
                intent: None,
            },
            created_at: chrono::Utc::now().to_rfc3339(),
            reason: None,
            evidence_ref: None,
            root_session_id: Some("approval-timeout-root".to_string()),
            workflow_id: Some(wf.workflow_id.clone()),
            task_id: Some(task_id.to_string()),
            status: None,
            decided_at: None,
            decided_by: None,
            decision_reason: None,
            approval_level: ApprovalLevel::Operator,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
            expires_at: None,
        };
        let gateway_dir = crate::execution::gateway_root_dir(&cfg);
        let store =
            Arc::new(crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap());
        store.create_approval(&mut approval).unwrap();

        let checkpoint = SessionCheckpoint {
            history: vec![],
            turn_counter: 1,
            loop_guard_state: crate::runtime::guard::LoopGuard::default(),
            session_state: autonoetic_types::agent::SessionState::default(),
            tool_tier_escalated: false,
            discovered_tools: std::collections::HashSet::new(),
            blocked_state_event_emitted: false,
            agent_id: "coder.default".to_string(),
            session_id: session_id.clone(),
            turn_id: "turn-1".to_string(),
            workflow_id: Some(wf.workflow_id.clone()),
            task_id: Some(task_id.to_string()),
            runtime_lock_hash: None,
            constitution_version: None,
            constitution_digest: None,
            llm_config_snapshot: None,
            tool_registry_version: None,
            yield_reason: YieldReason::ApprovalRequired {
                approval_request_id: approval_request_id.to_string(),
            },
            content_store_refs: vec![],
            created_at: chrono::Utc::now().to_rfc3339(),
            pending_tool_state: None,
            llm_rounds_consumed: 0,
            tool_invocations_consumed: 0,
            tokens_consumed: 0,
            estimated_cost_usd: 0.0,
            compression_metadata: None,
            capsule_state: None,
            assistant_message: None,
            pending_action: None,
            suspended_at: Some((chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339()),
            suppress_until_turn: 0,
            trajectory_last_level: None,
            feedback_events: vec![],
        };
        save_checkpoint(&cfg, &checkpoint).unwrap();

        run_approval_timeout_sweeper(&cfg).await;

        let updated = workflow_store::load_task_run(&cfg, None, &wf.workflow_id, task_id)
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, TaskRunStatus::Stale);

        // Checkpoint should still exist for late-approve resume.
        let cp = crate::runtime::checkpoint::load_latest_checkpoint(&cfg, &session_id).unwrap();
        assert!(
            cp.is_some(),
            "checkpoint must be preserved for late approval"
        );
    }
}

#[cfg(test)]
mod adjudication_sla_tests {
    use super::*;
    use autonoetic_types::config::{DeciderObligationsConfig, GatewayConfig};
    use crate::scheduler::gateway_store::anomaly_flags::AnomalyFlag;
    use crate::scheduler::gateway_store::constitutional_proposals::ConstitutionalProposal;
    use std::path::Path;
    use tempfile::tempdir;

    fn old_rfc3339(secs_ago: u64) -> String {
        (chrono::Utc::now() - chrono::Duration::seconds(secs_ago as i64)).to_rfc3339()
    }

    fn test_config(agents_dir: &Path, sla_secs: u64) -> GatewayConfig {
        GatewayConfig {
            agents_dir: agents_dir.to_path_buf(),
            decider_obligations: DeciderObligationsConfig {
                enabled: true,
                adjudication_sla_secs: sla_secs,
            },
            ..GatewayConfig::default()
        }
    }

    fn sample_proposal(proposal_id: &str) -> ConstitutionalProposal {
        ConstitutionalProposal {
            proposal_id: proposal_id.to_string(),
            proposer_agent_id: "auditor.default".to_string(),
            proposer_session_id: Some("sess-proposer".to_string()),
            kind: "add_right".to_string(),
            target_id: None,
            proposed_text: Some("Agents may do X".to_string()),
            justification: "closes a gap".to_string(),
            evidence_json: serde_json::json!([]),
            status: "pending".to_string(),
            operator_decision: None,
            decision_reason: None,
            decided_by: None,
            decided_at: None,
            published_in_release: None,
            created_at: old_rfc3339(1_000),
            sla_breached_at: None,
        }
    }

    fn sample_flag(flag_id: &str) -> AnomalyFlag {
        AnomalyFlag {
            flag_id: flag_id.to_string(),
            reporter_agent_id: "witness.default".to_string(),
            reporter_session_id: Some("sess-reporter".to_string()),
            subject_ref: "sess-target-1".to_string(),
            observation: "tool call bypassed policy check".to_string(),
            evidence_json: serde_json::json!([]),
            severity: "high".to_string(),
            status: "pending".to_string(),
            decision: None,
            decision_reason: None,
            decided_by: None,
            decided_at: None,
            created_at: old_rfc3339(1_000),
            sla_breached_at: None,
        }
    }

    #[tokio::test]
    async fn breaches_are_recorded_without_changing_status() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents, 100);

        let gateway_dir = cfg.agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store =
            std::sync::Arc::new(crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap());
        store
            .insert_constitutional_proposal(&sample_proposal("cprop-sla-1"))
            .unwrap();
        store.insert_anomaly_flag(&sample_flag("aflag-sla-1")).unwrap();

        let exec = std::sync::Arc::new(crate::execution::GatewayExecutionService::new(
            cfg,
            Some(store.clone()),
        ));

        check_adjudication_sla_breaches(exec).await.unwrap();

        // (b) sla_breached_at now set, (c) status unchanged.
        let proposal = store.get_constitutional_proposal("cprop-sla-1").unwrap().unwrap();
        assert!(proposal.sla_breached_at.is_some());
        assert_eq!(proposal.status, "pending");

        let flag = store.get_anomaly_flag("aflag-sla-1").unwrap().unwrap();
        assert!(flag.sla_breached_at.is_some());
        assert_eq!(flag.status, "pending");

        // (a) causal events tagged with the obligation IDs.
        let proposal_events = store
            .search_causal_events(None, Some("auditor.default"), 50)
            .unwrap();
        assert!(proposal_events.iter().any(|e| e.category == "decider_obligation"
            && e.action == "sla_breached"
            && e.enforced_rules == vec!["O-6".to_string()]
            && e.target.as_deref() == Some("cprop-sla-1")));

        let flag_events = store
            .search_causal_events(None, Some("witness.default"), 50)
            .unwrap();
        assert!(flag_events.iter().any(|e| e.category == "decider_obligation"
            && e.action == "sla_breached"
            && e.enforced_rules == vec!["O-7".to_string()]
            && e.target.as_deref() == Some("aflag-sla-1")));

        // (d) a notification row exists for each, addressed to "system"
        // (gateway-detected, not session-bound) with the owed party in the
        // payload's `owed_to`.
        let pending_notifications = store.list_pending_notifications().unwrap();
        assert!(pending_notifications.iter().any(|n| n.notification_type
            == autonoetic_types::notification::NotificationType::ConstitutionalProposal
            && n.target_session_id == "system"
            && n.payload.get("owed_to").and_then(|v| v.as_str()) == Some("auditor.default")));
        assert!(pending_notifications.iter().any(|n| n.notification_type
            == autonoetic_types::notification::NotificationType::AnomalyFlag
            && n.target_session_id == "system"
            && n.payload.get("owed_to").and_then(|v| v.as_str()) == Some("witness.default")));

        // Attribution contract: O-6/O-7 entered the code enforcement
        // register with the 2026.07.19 amendment, so the breach events must
        // attribute to their clauses in contract-health (not bucket as
        // `unattributed` as they did pre-enactment). This is the load-bearing
        // flip the amendment delivers — see `enforcement_register.rs`
        // entries for O-6 / O-7 and `docs/constitution/versions/2026.07.19/`.
        let health = store.contract_health(None).unwrap();
        let o6_count = health
            .by_clause
            .iter()
            .find(|(clause, _)| clause == "O-6")
            .map(|(_, n)| *n)
            .unwrap_or(0);
        let o7_count = health
            .by_clause
            .iter()
            .find(|(clause, _)| clause == "O-7")
            .map(|(_, n)| *n)
            .unwrap_or(0);
        assert!(o6_count >= 1, "O-6 breach should attribute to O-6 post-enactment; health: {:?}", health);
        assert!(o7_count >= 1, "O-7 breach should attribute to O-7 post-enactment; health: {:?}", health);
    }

    #[tokio::test]
    async fn second_tick_does_not_re_emit() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents, 100);

        let gateway_dir = cfg.agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store =
            std::sync::Arc::new(crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap());
        store
            .insert_constitutional_proposal(&sample_proposal("cprop-sla-2"))
            .unwrap();

        let exec = std::sync::Arc::new(crate::execution::GatewayExecutionService::new(
            cfg,
            Some(store.clone()),
        ));

        check_adjudication_sla_breaches(exec.clone()).await.unwrap();
        check_adjudication_sla_breaches(exec).await.unwrap();

        let events = store
            .search_causal_events(None, Some("auditor.default"), 50)
            .unwrap();
        let sla_events: Vec<_> = events
            .iter()
            .filter(|e| e.category == "decider_obligation" && e.action == "sla_breached")
            .collect();
        assert_eq!(sla_events.len(), 1, "breach must be recorded exactly once");
    }

    // ── #771 D.2: amendment invitations from denial telemetry ──

    fn invitation_test_config(
        agents_dir: &Path,
        threshold: u64,
        window_secs: u64,
    ) -> GatewayConfig {
        GatewayConfig {
            agents_dir: agents_dir.to_path_buf(),
            amendment_invitations: autonoetic_types::config::AmendmentInvitationConfig {
                enabled: true,
                threshold,
                window_secs,
            },
            ..GatewayConfig::default()
        }
    }

    fn push_denial(
        store: &crate::scheduler::gateway_store::GatewayStore,
        seq: &mut u64,
        agent_id: &str,
        rule_id: &str,
    ) {
        *seq += 1;
        let event = CausalEventRecord {
            event_id: format!("denial-ev-{seq}"),
            agent_id: agent_id.to_string(),
            session_id: "sess-friction".to_string(),
            turn_id: None,
            event_seq: *seq,
            timestamp: chrono::Utc::now().to_rfc3339(),
            category: "tool".to_string(),
            action: "failure".to_string(),
            status: "DENIED".to_string(),
            enforced_rules: vec![rule_id.to_string()],
            target: None,
            payload: None,
            payload_ref: None,
            evidence_ref: None,
            reason: None,
        };
        store.create_causal_event(&event).unwrap();
    }

    #[tokio::test]
    async fn invitation_issued_once_threshold_crossed() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = invitation_test_config(&agents, 3, 604800);

        let gateway_dir = cfg.agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
        );

        let mut seq = 0;
        // 3 denials of P-1.5 for coder (threshold = 3) — invitation expected.
        push_denial(&store, &mut seq, "coder.default", "P-1.5");
        push_denial(&store, &mut seq, "coder.default", "P-1.5");
        push_denial(&store, &mut seq, "coder.default", "P-1.5");
        // 2 denials of P-1.9 for coder (below threshold) — none.
        push_denial(&store, &mut seq, "coder.default", "P-1.9");
        push_denial(&store, &mut seq, "coder.default", "P-1.9");
        // 3 denials but spread across agents — none (same alias required).
        push_denial(&store, &mut seq, "planner.default", "P-7.5");
        push_denial(&store, &mut seq, "researcher.default", "P-7.5");

        let exec = std::sync::Arc::new(crate::execution::GatewayExecutionService::new(
            cfg,
            Some(store.clone()),
        ));

        check_amendment_invitation_thresholds(exec.clone()).await.unwrap();

        let open = store
            .list_amendment_invitations(Some("open"), Some("coder.default"), 64)
            .unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].rule_id, "P-1.5");
        assert_eq!(open[0].denial_count, 3);
        assert_eq!(open[0].threshold, 3);

        // (a) causal event carries Ri-0.8 + the friction statistics.
        let events = store
            .search_causal_events(None, Some("coder.default"), 50)
            .unwrap();
        let invitation_events: Vec<_> = events
            .iter()
            .filter(|e| e.category == "amendment_invitation" && e.action == "issued")
            .collect();
        assert_eq!(invitation_events.len(), 1);
        assert_eq!(
            invitation_events[0].enforced_rules,
            vec!["Ri-0.8".to_string()]
        );
        let payload: serde_json::Value =
            serde_json::from_str(invitation_events[0].payload.as_deref().unwrap()).unwrap();
        assert_eq!(payload["rule_id"], "P-1.5");
        assert_eq!(payload["denial_count"], 3);

        // (b) a notification row addressed to "system" with the invited
        // party in the payload (mirrors the SLA-breach pattern).
        let notifications = store.list_pending_notifications().unwrap();
        assert!(notifications.iter().any(|n| n.notification_type
            == autonoetic_types::notification::NotificationType::ConstitutionalProposal
            && n.target_session_id == "system"
            && n.payload.get("event").and_then(|v| v.as_str())
                == Some("amendment_invitation_issued")
            && n.payload.get("agent_id").and_then(|v| v.as_str()) == Some("coder.default")
            && n.payload.get("rule_id").and_then(|v| v.as_str()) == Some("P-1.5")));

        // (c) Ri-0.8 IS in the enforcement register, so the invitation
        // attributes in contract-health immediately (not `unattributed`).
        let health = store.contract_health(None).unwrap();
        assert!(health
            .by_clause
            .iter()
            .any(|(clause, _)| clause == "Ri-0.8"));

        // (d) second tick: the open invitation dedups — no new row, event,
        // or notification.
        check_amendment_invitation_thresholds(exec).await.unwrap();
        let open = store
            .list_amendment_invitations(Some("open"), Some("coder.default"), 64)
            .unwrap();
        assert_eq!(open.len(), 1);
        let events = store
            .search_causal_events(None, Some("coder.default"), 50)
            .unwrap();
        let invitation_events: Vec<_> = events
            .iter()
            .filter(|e| e.category == "amendment_invitation" && e.action == "issued")
            .collect();
        assert_eq!(invitation_events.len(), 1, "invitation must be issued exactly once");
    }

    #[tokio::test]
    async fn below_threshold_and_disabled_config_issue_nothing() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = invitation_test_config(&agents, 3, 604800);

        let gateway_dir = cfg.agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
        );

        let mut seq = 0;
        push_denial(&store, &mut seq, "coder.default", "P-1.5");
        push_denial(&store, &mut seq, "coder.default", "P-1.5");

        let exec = std::sync::Arc::new(crate::execution::GatewayExecutionService::new(
            cfg.clone(),
            Some(store.clone()),
        ));
        check_amendment_invitation_thresholds(exec).await.unwrap();
        assert!(store
            .list_amendment_invitations(Some("open"), Some("coder.default"), 64)
            .unwrap()
            .is_empty());

        // threshold = 0 disables issuance even with enough denials.
        let cfg_disabled = invitation_test_config(&agents, 0, 604800);
        push_denial(&store, &mut seq, "coder.default", "P-1.5");
        push_denial(&store, &mut seq, "coder.default", "P-1.5");
        push_denial(&store, &mut seq, "coder.default", "P-1.5");
        let exec_disabled = std::sync::Arc::new(crate::execution::GatewayExecutionService::new(
            cfg_disabled,
            Some(store.clone()),
        ));
        check_amendment_invitation_thresholds(exec_disabled)
            .await
            .unwrap();
        assert!(store
            .list_amendment_invitations(Some("open"), Some("coder.default"), 64)
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn expired_invitation_frees_pair_for_reissue() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        // Tiny window so the first invitation expires between ticks.
        let cfg = invitation_test_config(&agents, 1, 1);

        let gateway_dir = cfg.agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
        );

        // Seed an already-stale open invitation (created 1h ago with a 1s
        // window) — the tick must expire it.
        let stale = crate::scheduler::gateway_store::amendment_invitations::AmendmentInvitation {
            invitation_id: "ainv-stale".to_string(),
            agent_id: "coder.default".to_string(),
            rule_id: "P-1.5".to_string(),
            denial_count: 1,
            threshold: 1,
            window_secs: 1,
            status: "open".to_string(),
            answered_proposal_id: None,
            created_at: (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339(),
            resolved_at: None,
        };
        store.insert_amendment_invitation(&stale).unwrap();

        let mut seq = 0;
        push_denial(&store, &mut seq, "coder.default", "P-1.5");

        let exec = std::sync::Arc::new(crate::execution::GatewayExecutionService::new(
            cfg,
            Some(store.clone()),
        ));
        check_amendment_invitation_thresholds(exec).await.unwrap();

        let stale_row = store.get_amendment_invitation("ainv-stale").unwrap().unwrap();
        assert_eq!(stale_row.status, "expired");

        // The (agent, rule) pair was freed, so the fresh denial re-issued.
        let open = store
            .list_amendment_invitations(Some("open"), Some("coder.default"), 64)
            .unwrap();
        assert_eq!(open.len(), 1);
        assert_ne!(open[0].invitation_id, "ainv-stale");
    }
}
